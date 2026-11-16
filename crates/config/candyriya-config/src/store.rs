//! Hot reload via an atomic snapshot.
//!
//! [`ConfigStore`] owns the live [`Config`] behind an `ArcSwap`, so readers
//! never block and never see a half-updated config. Reload is performed by
//! re-running [`Config::load`]: on success the new config is swapped in
//! atomically and every future [`ConfigStore::get`] observes it; on failure
//! the old configuration remains fully intact and the error is returned.
//!
//! Reloading shares the exact same semantics as a fresh load: the forwarding
//! secret file is honored when present (an existing secret is kept, never
//! silently rotated), otherwise a new one is generated and a warning is
//! logged.

use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::Config;
use crate::error::ConfigError;

/// Shared, atomically swappable configuration handle.
pub struct ConfigStore {
    current: ArcSwap<Config>,
}

impl ConfigStore {
    /// Wraps an existing configuration.
    pub fn new(config: Config) -> Self {
        Self {
            current: ArcSwap::from_pointee(config),
        }
    }

    /// Loads the configuration from `path` and wraps it.
    ///
    /// Identical semantics to [`Config::load`], including forwarding-secret
    /// resolution relative to `path`'s directory.
    pub fn from_path(path: &Path) -> Result<Self, ConfigError> {
        Ok(Self::new(Config::load(path)?))
    }

    /// Returns the current configuration snapshot.
    ///
    /// The returned `Arc` keeps this exact version alive even if a reload
    /// swaps in a new one concurrently. Cheap: an atomic load plus a refcount
    /// bump, no locks.
    pub fn get(&self) -> Arc<Config> {
        self.current.load_full()
    }

    /// Alias for [`ConfigStore::get`] — a consistent point-in-time snapshot.
    pub fn snapshot(&self) -> Arc<Config> {
        self.get()
    }

    /// Reloads the configuration from `path` and atomically swaps it in.
    ///
    /// On error the current configuration is left untouched. On success the
    /// new snapshot is returned; the old one stays alive for any reader that
    /// already holds it. The forwarding secret is read from the secret file
    /// when present (never rotated implicitly); if it is missing one is
    /// generated and a warning is logged by the loader.
    pub fn reload(&self, path: &Path) -> Result<Arc<Config>, ConfigError> {
        let config = Config::load(path)?;
        let config = Arc::new(config);
        self.current.store(Arc::clone(&config));
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_config_round_trips() {
        let store = ConfigStore::new(Config::default());
        assert_eq!(store.get().config_version, "0.1");
        assert_eq!(store.snapshot().config_version, "0.1");
    }
}
