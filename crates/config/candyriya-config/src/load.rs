//! Loading and layering.
//!
//! Layer order, bottom to top: the embedded [`DEFAULT_CONFIG_TOML`]
//! template, then the user's file (if it exists) on top, merged key by key
//! with figment. Before anything is decoded, the user's TOML is parsed with
//! the `toml` crate so syntax errors carry line and column, and scanned
//! for unknown keys which are reported as warnings — never errors.

use std::path::Path;

use figment::Figment;
use figment::providers::{Format, Toml};

use crate::config::Config;
use crate::default::DEFAULT_CONFIG_TOML;
use crate::error::ConfigError;
use crate::secret::load_or_create_secret;
use crate::validate::validate;

/// Keys understood at the root of the file (scalars plus sections).
const ROOT_KEYS: &[&str] = &[
    "config-version",
    "bind",
    "motd",
    "show-max-players",
    "online-mode",
    "force-key-authentication",
    "player-info-forwarding-mode",
    "forwarding-secret-file",
    "prevent-client-proxy-connections",
    "enable-player-address-logging",
    "announce-forge",
    "kick-existing-players",
    "locale",
    "protocol",
    "servers",
    "forced-hosts",
    "advanced",
    "query",
    "permissions",
    "metrics",
    "motd-favicon",
    "plugins",
    "runtime",
];

/// Sections with a fixed set of keys. Tables with arbitrary user keys
/// (`[servers]` server entries, `[forced-hosts]` hostnames,
/// `[permissions.grants]` groups) are intentionally not listed — nothing
/// can be "unknown" there.
const FIXED_SECTIONS: &[(&str, &[&str])] = &[
    ("protocol", &["auto-update", "update-url"]),
    (
        "advanced",
        &[
            "compression-threshold",
            "compression-level",
            "login-ratelimit",
            "connection-timeout",
            "read-timeout",
            "haproxy-protocol",
            "tcp-fast-open",
            "bungee-plugin-message-channel",
            "show-ping-requests",
            "announce-proxy-commands",
            "failover-on-unexpected-server-disconnect",
            "log-command-executions",
            "log-player-connections",
            "accepts-transfers",
            "enable-reuse-port",
            "command-rate-limit",
            "tab-complete-rate-limit",
            "max-connections",
            "handshake-timeout",
            "max-new-connections-per-second",
        ],
    ),
    ("query", &["enabled", "port", "map", "show-plugins"]),
    ("permissions", &["provider", "default", "grants"]),
    ("metrics", &["prometheus", "prometheus-address"]),
    (
        "plugins",
        &[
            "max-memory-pages",
            "max-fuel",
            "max-calls",
            "max-call-timeout",
            "max-output-bytes",
            "max-input-bytes",
            "max-log-bytes",
        ],
    ),
    ("runtime", &["worker-threads", "blocking-threads"]),
];

/// Returns the dotted names of unknown keys, e.g. `["protocol.auto-updte"]`.
/// Non-table values in fixed sections are ignored here (figment reports
/// them as type errors instead).
pub(crate) fn unknown_keys(document: &toml::Value) -> Vec<String> {
    let mut unknown = Vec::new();
    let Some(root) = document.as_table() else {
        return unknown;
    };

    for (key, value) in root {
        if key == "servers" || key == "forced-hosts" {
            continue; // arbitrary names by design
        }
        match FIXED_SECTIONS.iter().find(|(section, _)| *section == key) {
            Some((_, known)) => {
                if let Some(table) = value.as_table() {
                    for sub in table.keys() {
                        if !known.contains(&sub.as_str()) {
                            unknown.push(format!("{key}.{sub}"));
                        }
                    }
                }
            }
            None => {
                if !ROOT_KEYS.contains(&key.as_str()) {
                    unknown.push(key.clone());
                }
            }
        }
    }
    unknown
}

/// Parses user TOML with the `toml` crate so syntax errors carry line and
/// column information (figment alone would not give us span details).
fn parse_user_toml(path: &Path, text: &str) -> Result<toml::Value, ConfigError> {
    toml::from_str::<toml::Value>(text).map_err(|error| ConfigError::Toml {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

impl Config {
    /// Loads the configuration from `path`.
    ///
    /// * A missing file is not an error: the embedded defaults are used and
    ///   a forwarding secret is still resolved relative to `path`'s
    ///   directory (created when absent).
    /// * A malformed file is a hard error whose message carries the TOML
    ///   parser's line and column.
    /// * Unknown keys are logged with `tracing::warn!` and ignored.
    /// * The forwarding secret file (resolved relative to `path`'s
    ///   directory unless absolute) is read, or generated with 32 random
    ///   bytes and mode 0600 on first use.
    /// * All detectable semantic problems are collected and reported as
    ///   [`ConfigError::Validation`].
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(
                    "config file {} not found; using embedded defaults",
                    path.display()
                );
                None
            }
            Err(error) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source: error,
                });
            }
        };

        let mut figment = Figment::new().merge(Toml::string(DEFAULT_CONFIG_TOML));
        if let Some(text) = text.as_deref() {
            let document = parse_user_toml(path, text)?;
            for key in unknown_keys(&document) {
                tracing::warn!(
                    "unknown configuration key `{key}` in {} (ignored)",
                    path.display()
                );
            }
            figment = figment.merge(Toml::string(text));
        }

        let mut config: Config = figment
            .extract()
            .map_err(|error| ConfigError::Decode(error.to_string()))?;

        let problems = validate(&config);
        if !problems.is_empty() {
            return Err(ConfigError::validation(problems));
        }

        config.forwarding_secret = load_or_create_secret(path, &config.forwarding_secret_file)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unknown_for(text: &str) -> Vec<String> {
        let document = toml::from_str::<toml::Value>(text).unwrap();
        unknown_keys(&document)
    }

    #[test]
    fn unknown_root_key_is_reported() {
        assert_eq!(unknown_for("bogus-key = 1\n"), vec!["bogus-key"]);
        assert_eq!(unknown_for("onlin-mode = false\n"), vec!["onlin-mode"]);
    }

    #[test]
    fn unknown_section_key_is_reported() {
        assert_eq!(
            unknown_for("[protocol]\nauto-updte = false\n"),
            vec!["protocol.auto-updte"]
        );
        assert_eq!(
            unknown_for("[advanced]\ncompression-threshld = 5\n"),
            vec!["advanced.compression-threshld"]
        );
    }

    #[test]
    fn arbitrary_table_keys_are_not_unknown() {
        let text = "[servers]\nlobby = \"127.0.0.1:25565\"\ntry = [\"lobby\"]\n";
        assert_eq!(unknown_for(text), Vec::<String>::new());
        let text = "[forced-hosts]\n\"play.example.com\" = [\"lobby\"]\n";
        assert_eq!(unknown_for(text), Vec::<String>::new());
        let text = "[permissions]\ngrants = { admin = [\"*\"] }\n";
        assert_eq!(unknown_for(text), Vec::<String>::new());
    }

    #[test]
    fn every_known_key_is_accepted() {
        let text = r#"
config-version = "0.1"
bind = "0.0.0.0:25565"
motd = "<aqua>x"
show-max-players = 100
motd-favicon = "assets/favicon.png"
online-mode = true
force-key-authentication = true
player-info-forwarding-mode = "modern"
forwarding-secret-file = "forwarding.secret"
prevent-client-proxy-connections = false
enable-player-address-logging = true
announce-forge = false
kick-existing-players = false
locale = "en"

[protocol]
auto-update = true
update-url = "https://example.com/protocolVersions.json"

[servers]
try = []

[forced-hosts]

[advanced]
compression-threshold = 256
compression-level = -1
login-ratelimit = 3000
connection-timeout = 3000
read-timeout = 30000
haproxy-protocol = false
tcp-fast-open = false
bungee-plugin-message-channel = true
show-ping-requests = false
announce-proxy-commands = true
failover-on-unexpected-server-disconnect = true
log-command-executions = false
log-player-connections = true
accepts-transfers = false
enable-reuse-port = false
command-rate-limit = 50
tab-complete-rate-limit = 10
max-connections = 1024
handshake-timeout = 5000
max-new-connections-per-second = 0

[query]
enabled = false
port = 25565
map = "candyriya"
show-plugins = false

[permissions]
provider = "config"
default = "default"

[metrics]
prometheus = false
prometheus-address = "0.0.0.0:9124"

[plugins]
max-memory-pages = 64
max-fuel = 0
max-calls = 2000
max-call-timeout = "5s"
max-output-bytes = 1048576
max-input-bytes = 16777216
max-log-bytes = 1048576

[runtime]
worker-threads = 4
blocking-threads = 4
"#;
        assert_eq!(unknown_for(text), Vec::<String>::new());
    }
}
