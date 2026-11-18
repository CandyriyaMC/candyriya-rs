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
use crate::ipc::{ServiceContext, ServiceWiring};
use crate::node::{ServiceKind, UnitKind};
use crate::pid::Pid;
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
    pub(crate) mailbox_capacity: Option<usize>,
    pub(crate) call_timeout: Option<Duration>,
    pub(crate) grants: Vec<Pid>,
    pub(crate) service_kind: Option<ServiceKind>,
    pub(crate) service: Option<Arc<dyn ServiceWiring>>,
}

impl std::fmt::Debug for UnitSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // the factory and the service wiring are opaque by construction;
        // print the shape, not the pointers
        f.debug_struct("UnitSpec")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("policy", &self.policy)
            .field("timeout", &self.timeout)
            .field("mailbox_capacity", &self.mailbox_capacity)
            .field("call_timeout", &self.call_timeout)
            .field("grants", &self.grants)
            .field("service_kind", &self.service_kind)
            .field("service", &self.service.as_ref().map(|_| ".."))
            .finish()
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
            mailbox_capacity: None,
            call_timeout: None,
            grants: Vec::new(),
            service_kind: None,
            service: None,
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

    /// Set the capacity of the unit's mailbox (default
    /// [`DEFAULT_MAILBOX_CAPACITY`](crate::DEFAULT_MAILBOX_CAPACITY), 256).
    ///
    /// The mailbox is bounded on purpose: when it is full, senders are
    /// rejected with [`CallError::Busy`](crate::CallError::Busy) immediately
    /// instead of queueing unbounded work or blocking a worker. Zero is
    /// clamped to 1.
    #[must_use]
    pub fn with_mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox_capacity = Some(capacity.max(1));
        self
    }

    /// Set the default timeout for calls this unit makes to other units
    /// (per-call [`CallOptions::timeout`](crate::CallOptions::timeout)
    /// overrides it).
    ///
    /// When unset, calls made by this unit fall back to
    /// [`DEFAULT_CALL_TIMEOUT`](crate::DEFAULT_CALL_TIMEOUT) (5s). The
    /// timeout covers the whole round trip: mailbox hand-off plus the
    /// handler's run time.
    #[must_use]
    pub fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = Some(timeout);
        self
    }

    /// Grant this unit a handle to `target` at spawn time, like an inherited
    /// fd.
    ///
    /// The grant is only installed if the *spawning* unit is allowed to pass
    /// it down, which is checked at spawn time: the spawner must be an
    /// ancestor of `target`, or itself hold a grant to `target`. A unit can
    /// never grant a handle to something it does not already reach, and a
    /// granted handle can only be passed further down (to the grantee's own
    /// children), never up or sideways. Granting the same target twice is a
    /// no-op.
    ///
    /// The caller identity at grant time is the parent passed to
    /// [`Init::spawn_unit`](crate::Init::spawn_unit); the spawner is
    /// responsible for claiming its true parent — the kernel cannot verify a
    /// bare pid (it is in-process, cooperative trust; see the crate docs).
    ///
    /// # Errors (at spawn time)
    ///
    /// The spawn fails with [`InitError::NoSuchUnit`](crate::InitError::NoSuchUnit)
    /// when `target` does not exist, and with
    /// [`InitError::GrantNotHeld`](crate::InitError::GrantNotHeld) when the
    /// spawner is not allowed to pass the handle down. Nothing is spawned in
    /// either case.
    #[must_use]
    pub fn with_grant(mut self, target: Pid) -> Self {
        if !self.grants.contains(&target) {
            self.grants.push(target);
        }
        self
    }

    /// Label the unit for the IPC metrics' `target` label.
    ///
    /// "Bounded" is enforced by the [`ServiceKind`] vocabulary, not by this
    /// builder: pick the kind that matches the service the unit fronts, or
    /// leave it unset and calls to the unit are reported as
    /// [`ServiceKind::Unit`].
    #[must_use]
    pub fn with_service_kind(mut self, service_kind: ServiceKind) -> Self {
        self.service_kind = Some(service_kind);
        self
    }

    /// Describe a new service unit straight from its handler.
    ///
    /// A service unit is what other units can call: the handler is invoked
    /// once per envelope in this unit's mailbox, in order, inside the unit's
    /// own body (so a cancel of the unit stops the handler loop at its next
    /// await). The handler receives a [`ServiceContext`] naming the unit and
    /// the caller, plus the request; whatever it returns is sent back to the
    /// caller as the response.
    ///
    /// The message pair (`Req`, `Resp`) is per service; downcast failures on
    /// the callee side surface to the caller as
    /// [`CallError::TypeMismatch`](crate::CallError::TypeMismatch) with a
    /// metric, never a panic. Handler panics are contained by the task
    /// service and surface to the caller as
    /// [`CallError::Panicked`](crate::CallError::Panicked).
    ///
    /// Units built this way do not need a separate body closure — the
    /// handler loop *is* the body, rebuilt on every restart. To attach a
    /// handler to a spec described otherwise, use
    /// [`UnitSpec::with_service`].
    pub fn service<Req, Resp, H, Fut>(
        name: impl Into<String>,
        kind: UnitKind,
        policy: RestartPolicy,
        handler: H,
    ) -> Self
    where
        Req: Send + 'static,
        Resp: Send + 'static,
        H: Fn(ServiceContext, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Resp> + Send + 'static,
    {
        Self::new(name, kind, policy, || {
            // never invoked: the wiring build takes over for service units
            std::future::pending::<ExitStatus>()
        })
        .with_service(handler)
    }

    /// Attach a service handler to this spec.
    ///
    /// Same semantics as [`UnitSpec::service`]; this builder form exists for
    /// specs described before the handler is decided. When a service wiring
    /// is attached, the body closure given to [`UnitSpec::new`] is ignored —
    /// the handler loop is the body.
    #[must_use]
    pub fn with_service<Req, Resp, H, Fut>(mut self, handler: H) -> Self
    where
        Req: Send + 'static,
        Resp: Send + 'static,
        H: Fn(ServiceContext, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Resp> + Send + 'static,
    {
        self.service = Some(Arc::new(crate::ipc::FnServiceWiring::new(handler)));
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

    /// The mailbox capacity, if one was set at build time.
    #[must_use]
    pub fn mailbox_capacity(&self) -> Option<usize> {
        self.mailbox_capacity
    }

    /// The default timeout for calls this unit makes, if one was set.
    #[must_use]
    pub fn call_timeout(&self) -> Option<Duration> {
        self.call_timeout
    }

    /// The grants this unit inherits at spawn time.
    #[must_use]
    pub fn grants(&self) -> &[Pid] {
        &self.grants
    }

    /// The service-kind label, if one was set.
    #[must_use]
    pub fn service_kind(&self) -> Option<ServiceKind> {
        self.service_kind
    }
}
