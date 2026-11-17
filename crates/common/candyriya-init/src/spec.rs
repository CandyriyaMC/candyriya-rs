//! How a unit's body is described to the tree.
//!
//! A unit's body is not one future but a *factory* producing futures, so a
//! unit with a restart policy can have its body re-created after each exit.
//! The factory is stored on the registry node; the tree calls it once at
//! spawn and once per restart. A one-shot body that cannot be re-created is
//! expressed as [`RestartPolicy::Never`](crate::RestartPolicy::Never), not by
//! making the factory non-callable.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::exit::ExitStatus;
use crate::node::UnitKind;
use crate::policy::RestartPolicy;

/// The future produced by a unit body factory.
///
/// The body reports its own outcome by returning an [`ExitStatus`]. Panicking
/// bodies are contained by the scheduler containment layer and surface as
/// [`ExitStatus::Panicked`]; bodies that are dropped before they return
/// (cancellation, or a per-unit timeout dropping the future mid-await) surface
/// as [`ExitStatus::Cancelled`] or [`ExitStatus::TimedOut`] respectively.
pub type UnitBody = Pin<Box<dyn Future<Output = ExitStatus> + Send>>;

/// Factory re-creating a unit's body.
///
/// Called at spawn and once per scheduled restart. Must be cheap to call:
/// it builds the future, it does not run it.
pub type UnitBodyFactory = Arc<dyn Fn() -> UnitBody + Send + Sync>;

/// A unit to register with the tree.
///
/// Construct with [`UnitSpec::new`], then adjust with the `with_*` builders.
/// Everything is validated at spawn time against the tree's current state.
pub struct UnitSpec {
    pub(crate) name: String,
    pub(crate) kind: UnitKind,
    pub(crate) policy: RestartPolicy,
    pub(crate) timeout: Option<Duration>,
    pub(crate) factory: UnitBodyFactory,
}

impl std::fmt::Debug for UnitSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // the factory is opaque by construction; print the shape, not the pointer
        f.debug_struct("UnitSpec")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("policy", &self.policy)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl UnitSpec {
    /// Describe a new unit.
    ///
    /// `body` is called once at spawn and once per restart the policy
    /// schedules; it must produce a fresh future each time.
    pub fn new<F, Fut>(
        name: impl Into<String>,
        kind: UnitKind,
        policy: RestartPolicy,
        body: F,
    ) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ExitStatus> + Send + 'static,
    {
        Self {
            name: name.into(),
            kind,
            policy,
            timeout: None,
            factory: Arc::new(move || Box::pin(body())),
        }
    }

    /// Bound how long a single run of the unit body may take.
    ///
    /// On expiry the body future is dropped (its destructors run) and the
    /// exit is recorded as [`ExitStatus::TimedOut`]. There is no default
    /// timeout: long-lived units would be killed by one, and one-shot
    /// bootstrap steps time out at the drain deadline instead.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// The unit's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The unit's kind.
    #[must_use]
    pub fn kind(&self) -> UnitKind {
        self.kind
    }

    /// The unit's restart policy.
    #[must_use]
    pub fn policy(&self) -> RestartPolicy {
        self.policy
    }

    /// The per-run timeout, if any.
    #[must_use]
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }
}
