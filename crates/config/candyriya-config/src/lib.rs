//! Validated TOML configuration for candyriya.
//!
//! Loads the proxy configuration with the same ergonomics as the classic
//! Minecraft proxy config files:
//!
//! * **Layered** — an embedded, fully commented [`DEFAULT_CONFIG_TOML`]
//!   provides every default; the user's file overrides only the keys it
//!   mentions. A missing file is legal and yields the embedded defaults.
//! * **Forgiving** — unknown keys are logged with `tracing::warn!` and
//!   ignored; they are never an error.
//! * **Not forgiving of garbage** — a malformed TOML file is a hard error
//!   whose message carries the parser's line and column; semantically
//!   invalid values are rejected with every detectable problem collected
//!   into [`ConfigError::Validation`].
//! * **Secret-aware** — the forwarding secret is resolved relative to the
//!   configuration file's directory; when missing, a fresh 32-byte secret
//!   is generated, base64-encoded, written with owner-only permissions,
//!   and a warning is logged. The secret never appears in debug output.
//! * **Hot-reloadable** — [`ConfigStore`] hands out immutable snapshots
//!   behind an `ArcSwap`; reloading swaps atomically and never leaves a
//!   half-updated config visible.
//!
//! # Loading
//!
//! ```no_run
//! use std::path::Path;
//! use candyriya_config::{Config, ConfigStore};
//!
//! let config = Config::load(Path::new("candyriya.toml")).expect("valid config");
//! println!("binding to {}", config.bind);
//!
//! let store = ConfigStore::from_path(Path::new("candyriya.toml")).expect("valid config");
//! let snapshot = store.get(); // cheap, lock-free
//! ```
//!
//! Everything is `#![forbid(unsafe_code)]` and nothing here panics in
//! production paths.
#![forbid(unsafe_code)]

mod config;
mod default;
mod duration;
mod error;
mod load;
mod secret;
mod store;
mod validate;

pub use config::{
    AdvancedConfig, Config, ForcedHosts, ForwardingMode, MetricsConfig, PermissionsConfig,
    PluginsConfig, ProtocolConfig, QueryConfig, RuntimeConfig, SUPPORTED_CONFIG_VERSION,
    ServersConfig,
};
pub use default::DEFAULT_CONFIG_TOML;
pub use error::{ConfigError, ValidationProblem, ValidationProblems};
pub use secret::{FORWARDING_SECRET_LEN, ForwardingSecret};
pub use store::ConfigStore;
