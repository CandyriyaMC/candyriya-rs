//! The registry of all backends the proxy knows about.
//!
//! [`BackendRegistry`] owns the backend set (name → address), the ordered
//! *try list* used for the login fallback chain, and the forced-hosts table.
//! It is built once from the `[servers]` and `[forced-hosts]` sections of the
//! configuration; the proxy hands it to the login pipeline.
//!
//! # Kick fallback
//!
//! When a backend kicks a player (or the initial connect fails), the login
//! flow must try the next backend in order. [`BackendRegistry::try_chain`]
//! returns the ordered names and [`BackendRegistry::next_try`] maps the
//! number of attempts so far onto the next untried backend:
//!
//! ```text
//! attempts = 0  -> first try entry
//! attempts = n  -> n-th try entry
//! attempts >= len -> None (chain exhausted, disconnect the player)
//! ```
//!
//! There is deliberately no modulo wrap: retrying a backend that already
//! kicked the player would be a retry loop, not a fallback. Health state is
//! advisory and does not remove a backend from the chain — the caller decides
//! whether to consult [`Backend::state`].
//!
//! # Forced hosts
//!
//! [`BackendRegistry::resolve_host`] answers "which backends may serve this
//! hostname" from the forced-hosts table; the login flow falls back to
//! [`BackendRegistry::try_chain`] when it returns `None`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use tracing::{debug, warn};

use crate::backend::{Backend, BackendState};
use crate::error::RouteError;
use crate::forced::ForcedHosts;
use crate::ping::status_ping;

/// Default timeout for a backend health check (connect + status ping).
pub const DEFAULT_HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// Longest accepted backend name (characters, not bytes).
const MAX_BACKEND_NAME_LEN: usize = 64;

/// One backend in the registry.
#[derive(Debug)]
pub struct BackendRegistry {
    backends: Vec<Backend>,
    by_name: HashMap<String, usize>,
    try_order: Vec<String>,
    forced: ForcedHosts,
}

impl BackendRegistry {
    /// Builds a registry from `(name, address)` pairs and the ordered try
    /// list from `[servers]` / `try = [...]`.
    ///
    /// Duplicate backend names and a try list that is empty, duplicates
    /// entries, or references unknown backends are all rejected — the
    /// configuration fails fast here instead of at the first login.
    ///
    /// The forced-hosts table starts empty; install one with
    /// [`BackendRegistry::set_forced_hosts`].
    ///
    /// Backends are kept in the order the pairs were given; [`BackendRegistry::iter`]
    /// follows that order (passing a `HashMap` makes it nondeterministic,
    /// which is the caller's choice).
    ///
    /// # Errors
    ///
    /// * [`RouteError::InvalidBackendName`] — a name is empty, too long, or
    ///   contains illegal characters;
    /// * [`RouteError::DuplicateBackend`] — the same name was registered twice;
    /// * [`RouteError::EmptyTryList`] — the try list is empty;
    /// * [`RouteError::UnknownTryBackend`] — the try list references a backend
    ///   that is not in the pairs;
    /// * [`RouteError::DuplicateTryBackend`] — the try list mentions a backend
    ///   twice.
    pub fn new(
        backends: impl IntoIterator<Item = (String, SocketAddr)>,
        try_order: impl IntoIterator<Item = String>,
    ) -> Result<Self, RouteError> {
        let mut stored = Vec::new();
        let mut by_name = HashMap::new();
        for (name, addr) in backends {
            validate_backend_name(&name)?;
            if by_name.contains_key(&name) {
                return Err(RouteError::DuplicateBackend(name));
            }
            by_name.insert(name.clone(), stored.len());
            stored.push(Backend::new(name, addr));
        }

        let mut order = Vec::new();
        for name in try_order {
            if !by_name.contains_key(&name) {
                return Err(RouteError::UnknownTryBackend(name));
            }
            if order.contains(&name) {
                return Err(RouteError::DuplicateTryBackend(name));
            }
            order.push(name);
        }
        if order.is_empty() {
            return Err(RouteError::EmptyTryList);
        }

        Ok(Self {
            backends: stored,
            by_name,
            try_order: order,
            forced: ForcedHosts::default(),
        })
    }

    /// Number of backends in the registry.
    pub fn len(&self) -> usize {
        self.backends.len()
    }

    /// Returns `true` when no backend is registered.
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    /// Looks up a backend by name.
    pub fn get(&self, name: &str) -> Option<&Backend> {
        self.by_name.get(name).map(|&index| &self.backends[index])
    }

    /// Iterates the backends in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &Backend> + '_ {
        self.backends.iter()
    }

    /// The last observed health state of a backend, or `None` if the name is
    /// not registered.
    pub fn state(&self, name: &str) -> Option<BackendState> {
        self.get(name).map(Backend::state)
    }

    /// Overwrites the health state of a backend.
    ///
    /// # Errors
    ///
    /// [`RouteError::UnknownBackend`] when `name` is not registered.
    pub fn set_state(&self, name: &str, state: BackendState) -> Result<(), RouteError> {
        match self.get(name) {
            Some(backend) => {
                backend.set_state(state);
                Ok(())
            }
            None => Err(RouteError::UnknownBackend(name.to_owned())),
        }
    }

    /// The ordered names of the try list, for the login flow.
    pub fn try_chain(&self) -> &[String] {
        &self.try_order
    }

    /// The next untried backend for the kick-fallback, given the number of
    /// attempts already made this connection (`0` = first try).
    ///
    /// Returns `None` once the chain is exhausted; callers disconnect the
    /// player at that point. Never wraps around.
    pub fn next_try(&self, completed: usize) -> Option<&Backend> {
        self.try_order
            .get(completed)
            .and_then(|name| self.get(name))
    }

    /// The backends allowed to serve `host` according to the forced-hosts
    /// table, or `None` when the host matches no pattern (fall back to
    /// [`BackendRegistry::try_chain`]).
    pub fn resolve_host(&self, host: &str) -> Option<&[String]> {
        self.forced.resolve(host)
    }

    /// The installed forced-hosts table.
    pub fn forced_hosts(&self) -> &ForcedHosts {
        &self.forced
    }

    /// Installs a forced-hosts table, validating that every backend it
    /// references exists in this registry.
    ///
    /// # Errors
    ///
    /// [`RouteError::UnknownForcedBackend`] when the table mentions a backend
    /// name that is not registered.
    pub fn set_forced_hosts(&mut self, forced: ForcedHosts) -> Result<(), RouteError> {
        for entry in forced.iter() {
            for backend in entry.backends() {
                if !self.by_name.contains_key(backend) {
                    return Err(RouteError::UnknownForcedBackend {
                        host: entry.pattern().to_owned(),
                        backend: backend.clone(),
                    });
                }
            }
        }
        self.forced = forced;
        Ok(())
    }

    /// Health-checks every backend with [`DEFAULT_HEALTH_CHECK_TIMEOUT`].
    ///
    /// Checks run sequentially: this crate never spawns ad-hoc tasks, the
    /// caller (typically the scheduler) decides when and how often to call
    /// this. Backends are updated in place; the returned outcomes describe
    /// what happened for metrics and logs.
    pub async fn health_check(&self) -> Vec<HealthCheckOutcome> {
        self.health_check_with_timeout(DEFAULT_HEALTH_CHECK_TIMEOUT)
            .await
    }

    /// Health-checks every backend, each with `timeout` for connect + ping.
    pub async fn health_check_with_timeout(&self, timeout: Duration) -> Vec<HealthCheckOutcome> {
        let mut outcomes = Vec::with_capacity(self.backends.len());
        for backend in &self.backends {
            outcomes.push(self.check_one(backend, timeout).await);
        }
        outcomes
    }

    /// Health-checks a single backend by name.
    ///
    /// # Errors
    ///
    /// [`RouteError::UnknownBackend`] when `name` is not registered. Ping
    /// failures are *not* errors here — they come back as an outcome with
    /// `state: Offline` and the error attached.
    pub async fn health_check_backend(
        &self,
        name: &str,
        timeout: Duration,
    ) -> Result<HealthCheckOutcome, RouteError> {
        let backend = self
            .get(name)
            .ok_or_else(|| RouteError::UnknownBackend(name.to_owned()))?;
        Ok(self.check_one(backend, timeout).await)
    }

    async fn check_one(&self, backend: &Backend, timeout: Duration) -> HealthCheckOutcome {
        match status_ping(backend.addr(), timeout).await {
            Ok(outcome) => {
                backend.mark_checked(BackendState::Online);
                debug!(backend = %backend.name(), addr = %backend.addr(), motd = %outcome.motd, "backend is online");
                HealthCheckOutcome {
                    name: backend.name().to_owned(),
                    state: BackendState::Online,
                    motd: Some(outcome.motd),
                    error: None,
                }
            }
            Err(error) => {
                backend.mark_checked(BackendState::Offline);
                warn!(backend = %backend.name(), addr = %backend.addr(), error = %error, "backend health check failed");
                HealthCheckOutcome {
                    name: backend.name().to_owned(),
                    state: BackendState::Offline,
                    motd: None,
                    error: Some(error),
                }
            }
        }
    }
}

/// What happened during a backend health check.
#[derive(Debug)]
pub struct HealthCheckOutcome {
    /// Name of the backend that was checked.
    pub name: String,
    /// The state the backend was marked with as a result of the check.
    pub state: BackendState,
    /// The first [`crate::MOTD_PREVIEW_BYTES`] of the status JSON, if the
    /// backend answered with one.
    pub motd: Option<String>,
    /// The error that made the backend offline, if any. Kept for logging and
    /// metrics; the backend is marked [`BackendState::Offline`] either way.
    pub error: Option<RouteError>,
}

impl HealthCheckOutcome {
    /// `true` when the backend answered with a valid status response.
    pub fn healthy(&self) -> bool {
        self.state == BackendState::Online
    }
}

/// A backend name: non-empty, at most [`MAX_BACKEND_NAME_LEN`] characters
/// of Unicode alphanumerics plus `-`, `_` and `.`.
fn validate_backend_name(name: &str) -> Result<(), RouteError> {
    let valid = !name.is_empty()
        && name.chars().count() <= MAX_BACKEND_NAME_LEN
        && name
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(RouteError::InvalidBackendName(name.to_owned()))
    }
}
