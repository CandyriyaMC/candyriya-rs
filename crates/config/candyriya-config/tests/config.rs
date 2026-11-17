//! Behaviour coverage for `candyriya-config`.
//!
//! No network access anywhere: every test works in a temporary directory
//! with files written locally.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use candyriya_config::{
    AdvancedConfig, Config, ConfigError, ConfigStore, DEFAULT_CONFIG_TOML, ForwardingMode,
    PluginsConfig, SUPPORTED_CONFIG_VERSION,
};

fn temp_config(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("candyriya.toml");
    std::fs::write(&path, contents).expect("write test config");
    path
}

fn secret_bytes(dir: &Path) -> Vec<u8> {
    let raw = std::fs::read_to_string(dir.join("forwarding.secret")).expect("read secret file");
    base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .expect("secret file must be base64")
}

#[test]
fn embedded_default_is_valid_and_loads_without_a_file() {
    // sanity: the template parses as TOML and extracts to Config::default
    let value: toml::Value =
        toml::from_str(DEFAULT_CONFIG_TOML).expect("template must be valid TOML");
    assert!(value.is_table());

    // a missing file is not an error: defaults are used (documented behavior)
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("candyriya.toml");
    let config = Config::load(&path).expect("missing file must load embedded defaults");
    assert_eq!(config.bind, "0.0.0.0:25565");
    assert_eq!(config.config_version, SUPPORTED_CONFIG_VERSION);
    assert_eq!(config.motd, "<aqua>A Candyriya network");
    assert_eq!(config.player_info_forwarding_mode, ForwardingMode::Modern);
    assert_eq!(config.advanced, AdvancedConfig::default());
    // runtime defaults are the CPU count and must be at least 1
    assert!(config.runtime.worker_threads >= 1);
    assert!(config.runtime.blocking_threads >= 1);
    // the default load still resolves (and generates) a forwarding secret
    assert!(temp.path().join("forwarding.secret").exists());
}

#[test]
fn full_valid_file_parses() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(
        temp.path(),
        r#"
config-version = "0.1"
bind = "play.example.com:25566"
motd = "<aqua>custom net"
show-max-players = 250
online-mode = false
force-key-authentication = false
player-info-forwarding-mode = "bungeeguard"
forwarding-secret-file = "secrets/custom.secret"
prevent-client-proxy-connections = true
enable-player-address-logging = false
announce-forge = true
kick-existing-players = true
locale = "ex_girl"

[protocol]
auto-update = false
update-url = "https://example.com/protocolVersions.json"

[servers]
try = ["minigames", "lobby"]
lobby = "127.0.0.1:25565"
minigames = "127.0.0.1:25566"

[forced-hosts]
"play.example.com" = ["lobby"]
"minigames.example.com" = ["minigames", "lobby"]

[advanced]
compression-threshold = -1
compression-level = 9
login-ratelimit = 1000
connection-timeout = 2000
read-timeout = 10000
haproxy-protocol = true
tcp-fast-open = true
bungee-plugin-message-channel = false
show-ping-requests = true
announce-proxy-commands = false
failover-on-unexpected-server-disconnect = false
log-command-executions = true
log-player-connections = false
accepts-transfers = true
enable-reuse-port = true
command-rate-limit = 100
tab-complete-rate-limit = 20
max-connections = 4096
handshake-timeout = 10000
max-new-connections-per-second = 42

[query]
enabled = true
port = 25570
map = "custommap"
show-plugins = true

[permissions]
provider = "custom-provider"
default = "vip"

[permissions.grants]
admin = ["*"]
moderator = ["candyriya.command.*", "candyriya.kick"]

[metrics]
prometheus = true
prometheus-address = "0.0.0.0:9155"

[runtime]
worker-threads = 3
blocking-threads = 2
"#,
    );

    let config = Config::load(&path).expect("full valid file must parse");
    assert_eq!(config.bind, "play.example.com:25566");
    assert_eq!(config.motd, "<aqua>custom net");
    assert!(!config.online_mode);
    assert_eq!(
        config.player_info_forwarding_mode,
        ForwardingMode::BungeeGuard
    );
    assert_eq!(config.forwarding_secret_file, "secrets/custom.secret");
    assert_eq!(
        config.protocol.update_url,
        "https://example.com/protocolVersions.json"
    );
    assert_eq!(config.servers.try_servers, vec!["minigames", "lobby"]);
    assert_eq!(
        config.servers.servers.get("lobby").map(String::as_str),
        Some("127.0.0.1:25565")
    );
    assert_eq!(
        config
            .forced_hosts
            .get("minigames.example.com")
            .map(Vec::as_slice),
        Some(&["minigames".to_string(), "lobby".to_string()][..])
    );
    assert_eq!(config.advanced.compression_threshold, -1);
    assert_eq!(config.advanced.compression_level, 9);
    assert_eq!(config.advanced.max_connections, 4096);
    assert_eq!(config.advanced.max_new_connections_per_second, 42);
    assert!(config.query.enabled);
    assert_eq!(config.query.port, 25570);
    assert_eq!(
        config.permissions.grants["moderator"],
        ["candyriya.command.*", "candyriya.kick"]
    );
    assert!(config.metrics.prometheus);
    assert_eq!(config.runtime.worker_threads, 3);
    assert_eq!(config.runtime.blocking_threads, 2);
    // generation warning happened but the custom secret path now exists
    assert!(temp.path().join("secrets").join("custom.secret").exists());
}

#[test]
fn try_referencing_unknown_server_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(
        temp.path(),
        "[servers]\nlobby = \"127.0.0.1:25565\"\ntry = [\"lobby\", \"nowhere\"]\n",
    );
    let error = Config::load(&path).unwrap_err();
    let problems = error.problems();
    assert!(
        problems
            .iter()
            .any(|p| p.field == "servers.try" && p.message.contains("nowhere")),
        "expected a servers.try problem mentioning `nowhere`, got: {problems:?}"
    );
}

#[test]
fn servers_without_try_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(temp.path(), "[servers]\nlobby = \"127.0.0.1:25565\"\n");
    let error = Config::load(&path).unwrap_err();
    let problems = error.problems();
    assert!(
        problems.iter().any(|p| p.field == "servers.try"),
        "expected a servers.try problem, got: {problems:?}"
    );
}

#[test]
fn forced_host_referencing_unknown_server_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(
        temp.path(),
        "[servers]\nlobby = \"127.0.0.1:25565\"\ntry = [\"lobby\"]\n\n[forced-hosts]\n\"play.example.com\" = [\"lobby\", \"ghost\"]\n",
    );
    let error = Config::load(&path).unwrap_err();
    let problems = error.problems();
    assert!(
        problems
            .iter()
            .any(|p| p.field.contains("forced-hosts") && p.message.contains("ghost")),
        "expected a forced-hosts problem mentioning `ghost`, got: {problems:?}"
    );
}

#[test]
fn port_zero_and_port_70000_are_rejected() {
    for bind in [
        "0.0.0.0:0",
        "0.0.0.0:70000",
        "[::1]:0",
        "play.example.com:99999",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp_config(temp.path(), &format!("bind = \"{bind}\"\n"));
        let error = Config::load(&path).unwrap_err();
        assert!(
            error.problems().iter().any(|p| p.field == "bind"),
            "expected a bind problem for `{bind}`, got: {error:?}"
        );
    }

    for port in ["0", "70000"] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp_config(temp.path(), &format!("[query]\nport = {port}\n"));
        let error = Config::load(&path).unwrap_err();
        assert!(
            error.problems().iter().any(|p| p.field == "query.port"),
            "expected a query.port problem for port {port}, got: {error:?}"
        );
    }

    // and prometheus-address is validated as host:port as well
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(
        temp.path(),
        "[metrics]\nprometheus-address = \"0.0.0.0:70000\"\n",
    );
    let error = Config::load(&path).unwrap_err();
    assert!(
        error
            .problems()
            .iter()
            .any(|p| p.field == "metrics.prometheus-address")
    );
}

#[test]
fn compression_level_out_of_bounds_is_rejected() {
    for level in ["10", "-2", "100"] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp_config(
            temp.path(),
            &format!("[advanced]\ncompression-level = {level}\n"),
        );
        let error = Config::load(&path).unwrap_err();
        assert!(
            error
                .problems()
                .iter()
                .any(|p| p.field == "advanced.compression-level"),
            "expected an advanced.compression-level problem for level {level}, got: {error:?}"
        );
    }
}

#[test]
fn compression_threshold_below_minus_one_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(temp.path(), "[advanced]\ncompression-threshold = -2\n");
    let error = Config::load(&path).unwrap_err();
    assert!(
        error
            .problems()
            .iter()
            .any(|p| p.field == "advanced.compression-threshold")
    );
}

#[test]
fn missing_secret_is_generated_and_reload_keeps_it() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("candyriya.toml"); // intentionally not written: defaults are fine

    let config = Config::load(&path).expect("first load generates the secret");
    let secret_path = temp.path().join("forwarding.secret");
    assert!(
        secret_path.exists(),
        "secret file must be created next to the config file"
    );

    let bytes = secret_bytes(temp.path());
    assert_eq!(bytes.len(), 32, "generated secret must be 32 bytes");
    assert_eq!(config.forwarding_secret.as_bytes(), bytes.as_slice());

    // raw file must be base64 (an operator can eyeball it), newline-terminated
    let raw = std::fs::read_to_string(&secret_path).unwrap();
    assert!(
        raw.trim()
            .ends_with(|c: char| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
    );
    assert!(raw.ends_with('\n'));

    // a reload must keep the existing secret, never rotate it implicitly
    let reloaded = Config::load(&path).expect("reload works");
    assert_eq!(reloaded.forwarding_secret.as_bytes(), bytes.as_slice());

    // deleting the secret and reloading generates a fresh one
    std::fs::remove_file(&secret_path).unwrap();
    let regenerated = Config::load(&path).expect("reload after deletion generates a new secret");
    let fresh = secret_bytes(temp.path());
    assert_eq!(fresh.len(), 32);
    assert_eq!(regenerated.forwarding_secret.as_bytes(), fresh.as_slice());
    assert_ne!(
        &fresh, &bytes,
        "a newly generated secret must not equal the old one"
    );
}

#[test]
fn existing_malformed_secret_is_a_hard_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(temp.path(), "");
    std::fs::write(temp.path().join("forwarding.secret"), "!!! not base64 !!!").unwrap();
    let error = Config::load(&path).unwrap_err();
    assert!(
        matches!(error, ConfigError::Secret(_)),
        "expected a Secret error, got: {error:?}"
    );
}

#[test]
fn existing_wrong_size_secret_is_a_hard_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(temp.path(), "");
    // 16 zero bytes, valid base64 but not 32 bytes
    std::fs::write(
        temp.path().join("forwarding.secret"),
        base64::engine::general_purpose::STANDARD.encode([0u8; 16]),
    )
    .unwrap();
    let error = Config::load(&path).unwrap_err();
    assert!(error.to_string().contains("expected 32"), "got: {error}");
}

#[test]
fn unknown_keys_warn_and_never_fail() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(
        temp.path(),
        "bogus-key = 123\nonlin-mode = false\n\n[advanced]\ncompression-threshld = 5\n",
    );

    // capture tracing output to assert the warnings are emitted
    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let config = Config::load(&path);
        assert!(
            config.is_ok(),
            "unknown keys must never fail the load: {config:?}"
        );
    });

    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("bogus-key"),
        "expected a warning naming bogus-key, got:\n{logs}"
    );
    assert!(
        logs.contains("onlin-mode"),
        "expected a warning naming onlin-mode, got:\n{logs}"
    );
    assert!(
        logs.contains("advanced.compression-threshld"),
        "expected a warning naming the advanced key, got:\n{logs}"
    );
}

#[test]
fn malformed_toml_reports_line_and_column() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(temp.path(), "bind = \n");
    let error = Config::load(&path).unwrap_err();
    let message = error.to_string();
    assert!(
        matches!(error, ConfigError::Toml { .. }),
        "expected a Toml error, got: {error:?}"
    );
    assert!(
        message.contains("line"),
        "expected line information, got: {message}"
    );
    assert!(
        message.contains("column"),
        "expected column information, got: {message}"
    );

    // broken structure inside a table is equally a line-carrying error
    let path = temp_config(
        temp.path(),
        "bind = \"0.0.0.0:25565\"\n[advanced\ncompression-level = 5\n",
    );
    let error = Config::load(&path).unwrap_err();
    assert!(matches!(error, ConfigError::Toml { .. }));
    assert!(error.to_string().contains("line"));
}

#[test]
fn reload_swaps_atomically_and_keeps_old_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(temp.path(), "motd = \"<aqua>version one\"\n");

    let store = ConfigStore::from_path(&path).expect("load v1");
    let old = store.get();
    assert_eq!(old.motd, "<aqua>version one");

    std::fs::write(&path, "motd = \"<aqua>version two\"\n").unwrap();
    let new = store.reload(&path).expect("reload to v2");
    assert_eq!(new.motd, "<aqua>version two");
    assert_eq!(store.get().motd, "<aqua>version two");
    assert_eq!(
        old.motd, "<aqua>version one",
        "held snapshots must not observe the swap"
    );

    // a failing reload leaves the current config fully intact
    std::fs::write(&path, "[advanced]\ncompression-level = 10\n").unwrap();
    let error = store.reload(&path).unwrap_err();
    assert!(matches!(error, ConfigError::Validation(_)));
    assert_eq!(store.get().motd, "<aqua>version two");
}

#[test]
fn debug_never_leaks_the_secret() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(temp.path(), "");
    let config = Config::load(&path).unwrap();

    let secret_b64 =
        base64::engine::general_purpose::STANDARD.encode(config.forwarding_secret.as_bytes());
    let secret_hex: String = config
        .forwarding_secret
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let secret_dec: String = config
        .forwarding_secret
        .as_bytes()
        .iter()
        .map(|b| b.to_string())
        .collect();

    let debug = format!("{config:?}");
    assert!(
        !debug.contains(secret_b64.trim_end_matches('=')),
        "base64 secret leaked in Debug output"
    );
    assert!(
        !debug.contains(&secret_hex),
        "hex secret leaked in Debug output"
    );
    assert!(
        !debug.contains(&secret_dec),
        "raw secret bytes leaked in Debug output"
    );
    assert!(debug.contains("***"), "Debug must show the redacted marker");
}

#[test]
fn grants_structure_is_accepted_and_validated() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(
        temp.path(),
        "[permissions.grants]\nadmin = [\"*\"]\nmoderator = [\"candyriya.command.*\", \"candyriya.kick\"]\n",
    );
    let config = Config::load(&path).expect("valid grants must load");
    assert_eq!(config.permissions.grants["admin"], ["*"]);
    assert_eq!(config.permissions.grants["moderator"].len(), 2);

    // inline-table form is equivalent
    let path = temp_config(
        temp.path(),
        "[permissions]\ngrants = { vip = [\"candyriya.fly\"] }\n",
    );
    let config = Config::load(&path).expect("inline grants must load");
    assert_eq!(config.permissions.grants["vip"], ["candyriya.fly"]);

    // wrong value type: not a list of strings — hard decode error
    let path = temp_config(temp.path(), "[permissions.grants]\nadmin = \"*\"\n");
    let error = Config::load(&path).unwrap_err();
    assert!(
        matches!(error, ConfigError::Decode(_)),
        "expected a decode error, got: {error:?}"
    );

    // empty permission nodes are rejected by validation
    let path = temp_config(temp.path(), "[permissions.grants]\nadmin = [\"\"]\n");
    let error = Config::load(&path).unwrap_err();
    assert!(
        error
            .problems()
            .iter()
            .any(|p| p.field.starts_with("permissions.grants"))
    );
}

#[test]
fn config_version_mismatch_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(temp.path(), "config-version = \"0.2\"\n");
    let error = Config::load(&path).unwrap_err();
    assert!(error.problems().iter().any(|p| p.field == "config-version"));
}

#[test]
fn control_characters_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    // TOML escape for a bell character inside the bind string
    let path = temp_config(temp.path(), "bind = \"0.0.0.0:25\\u000765\"\n");
    let error = Config::load(&path).unwrap_err();
    assert!(
        error.problems().iter().any(|p| p.field == "bind"),
        "expected a bind problem, got: {error:?}"
    );

    let path = temp_config(temp.path(), "motd = \"<aqua>bell\\u0007here\"\n");
    let error = Config::load(&path).unwrap_err();
    assert!(
        error.problems().iter().any(|p| p.field == "motd"),
        "expected a motd problem, got: {error:?}"
    );
}

#[test]
fn multiple_problems_are_collected_into_one_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(
        temp.path(),
        "bind = \"nonsense\"\nconfig-version = \"9.9\"\n[servers]\nlobby = \"127.0.0.1:25565\"\ntry = [\"nope\"]\n",
    );
    let error = Config::load(&path).unwrap_err();
    let problems = error.problems();
    assert!(
        problems.len() >= 3,
        "expected at least 3 collected problems, got: {problems:?}"
    );
}

#[test]
fn plugins_section_parses_and_defaults_apply() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(
        temp.path(),
        "[plugins]\nmax-memory-pages = 32\nmax-fuel = 1000000\nmax-calls = 100\nmax-call-timeout = \"7s\"\nmax-output-bytes = 2048\nmax-input-bytes = 4096\nmax-log-bytes = 128\n",
    );
    let config = Config::load(&path).expect("plugins section loads");
    assert_eq!(config.plugins.max_memory_pages, 32);
    assert_eq!(config.plugins.max_fuel, 1_000_000);
    assert_eq!(config.plugins.max_calls, 100);
    assert_eq!(config.plugins.max_call_timeout, Duration::from_secs(7));
    assert_eq!(config.plugins.max_output_bytes, 2048);
    assert_eq!(config.plugins.max_input_bytes, 4096);
    assert_eq!(config.plugins.max_log_bytes, 128);

    // a millisecond duration unit must decode too
    let path = temp_config(temp.path(), "[plugins]\nmax-call-timeout = \"250ms\"\n");
    let config = Config::load(&path).expect("millisecond durations parse");
    assert_eq!(config.plugins.max_call_timeout, Duration::from_millis(250));

    // missing section falls back to the documented defaults
    let path = temp_config(temp.path(), "");
    let config = Config::load(&path).expect("empty config loads");
    assert_eq!(config.plugins, PluginsConfig::default());
}

#[test]
fn plugins_rejects_bad_duration_and_zero_values() {
    // a duration that does not match the grammar is a decode error
    let temp = tempfile::tempdir().unwrap();
    let path = temp_config(temp.path(), "[plugins]\nmax-call-timeout = \"whenever\"\n");
    let error = Config::load(&path).unwrap_err();
    assert!(
        matches!(error, ConfigError::Decode(_)),
        "expected a decode error, got: {error:?}"
    );
    assert!(
        error.to_string().contains("ms"),
        "error must name the expected units: {error}"
    );

    // zero memory pages cannot run a plugin at all
    let path = temp_config(temp.path(), "[plugins]\nmax-memory-pages = 0\n");
    let error = Config::load(&path).unwrap_err();
    assert!(
        error
            .problems()
            .iter()
            .any(|p| p.field == "plugins.max-memory-pages"),
        "expected a plugins.max-memory-pages problem, got: {error:?}"
    );

    // a zero timeout traps every call instantly
    let path = temp_config(temp.path(), "[plugins]\nmax-call-timeout = \"0s\"\n");
    let error = Config::load(&path).unwrap_err();
    assert!(
        error
            .problems()
            .iter()
            .any(|p| p.field == "plugins.max-call-timeout"),
        "expected a plugins.max-call-timeout problem, got: {error:?}"
    );

    // zero fuel and zero calls mean unlimited and must load
    let path = temp_config(temp.path(), "[plugins]\nmax-fuel = 0\nmax-calls = 0\n");
    Config::load(&path).expect("0 means unlimited and must load");
}
