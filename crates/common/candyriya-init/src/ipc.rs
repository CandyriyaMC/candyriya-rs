//! The capability-IPC plane: every cross-unit call goes through the tree.
//!
//! Units never call each other's code or hold each other's handles. The only
//! sanctioned path is [`Init::call`], which:
//!
//! 1. resolves the target in the registry (one short critical section, no
//!    lock held across an `.await`),
//! 2. checks the capability — the caller is an *ancestor* of the target
//!    (subtree rule) or holds an explicit *grant* to it,
//! 3. hands the message over with [`tokio::sync::mpsc::Sender::try_send`]
//!    into the target's bounded mailbox,
//! 4. waits for the typed reply with a timeout and maps the outcome onto
//!    [`CallError`].
//!
//! # The mailbox model
//!
//! Every unit owns a bounded mailbox, created when the unit's body run is
//! prepared (spawn, or each scheduled restart). The default capacity is
//! [`DEFAULT_MAILBOX_CAPACITY`] (256); the knob is
//! [`UnitSpec::with_mailbox_capacity`](crate::UnitSpec::with_mailbox_capacity).
//! A full mailbox is never queue-bombed: the call fails immediately with
//! [`CallError::Busy`]. A unit without a service handler owns a mailbox
//! whose receiver is dropped at build time, so calls fail fast with
//! [`CallError::NotRunning`] instead of hanging until the timeout.
//!
//! Delivery itself happens inside the target's body, which runs on the
//! central task service: the handler loop is part of the unit body, so a
//! cancel of the unit stops it at its next await. No call ever spawns an
//! ad-hoc task.
//!
//! # The capability model
//!
//! Authorization is checked at call time against the registry:
//!
//! - **subtree rule** — the caller is an ancestor of the target (the walk
//!   goes up the ppid chain, depth ≤ the tree's ~8), or the caller *is* the
//!   target (self-calls are allowed), or the caller is [`Pid::INIT`];
//! - **grants** — rows in a kernel table `(holder, target)`. The table is
//!   written in exactly one place (spawn, from `UnitSpec::with_grant`) and
//!   only when the spawner is allowed to pass the handle down: the spawner
//!   is an ancestor of the target, or the spawner itself holds a grant to
//!   the target. That is what makes grants capabilities inherited like fds:
//!   a unit can never mint a handle for something it doesn't already reach,
//!   and a handle passes further down only (into the grantee's own
//!   children), never up or sideways. There is deliberately no dynamic
//!   re-grant API and no client-side token object: the table is the proof
//!   object, and in-process code is trusted (see below).
//!
//! Grants to dead targets or from dead holders are pruned when a node is
//! removed from the registry (reaped). Nodes that restart keep their pid
//! and their grants.
//!
//! # Trust model (honest version)
//!
//! Authorization is evaluated against the *claimed* caller pid. Inside the
//! process, host code is trusted (the same cooperative-trust assumption the
//! whole tree makes); a unit that passes a foreign pid gains exactly that
//! unit's rights. The kernel cannot seal pids in memory, and pretending
//! otherwise would be theatre. What the kernel *does* enforce mechanically
//! is that grants cannot be conjured: they appear only through the two
//! spawn-time checks above, and every call goes through the table or the
//! ancestry walk — there is no third path. Unforgeable capabilities arrive
//! with the plugin boundary (out-of-process/WASM), not here.
//!
//! # In-flight calls and cancellation
//!
//! There is exactly one observable shape for "the target went away while
//! answering": the reply `oneshot::Sender` is dropped without a send, which
//! happens when the handler panics (caught by the task service, exit
//! `Panicked`) or when the unit is stopped mid-call (the body future is
//! dropped at its next await point). Both surface to the caller as
//! [`CallError::Panicked`]. A target that is dead *before* the call starts
//! fails fast with [`CallError::NotRunning`]; a full mailbox fails fast
//! with [`CallError::Busy`]; only a live-but-slow callee burns the call
//! timeout. Callers that are themselves cancelled mid-call simply drop the
//! waiting future — there is nothing left to deliver to.
//!
//! # What is never logged
//!
//! One structured event per call, carrying caller/target/outcome/duration
//! only. Request and response payloads are never logged: values may carry
//! secrets, and this plane is not a debugger for what rides on it.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::exit::ExitStatus;
use crate::node::{ServiceKind, UnitState};
use crate::pid::Pid;
use crate::spec::UnitBody;
use crate::tree::{Init, TreeState};

/// Default capacity of a unit's mailbox.
///
/// 256 envelopes in flight per unit is far past any boot-time call rate and
/// far under memory pressure; tighten per unit with
/// [`UnitSpec::with_mailbox_capacity`](crate::UnitSpec::with_mailbox_capacity).
pub const DEFAULT_MAILBOX_CAPACITY: usize = 256;

/// Default timeout for a unit-to-unit call, round trip included.
///
/// Overridable globally per unit ([`UnitSpec::with_call_timeout`](crate::UnitSpec::with_call_timeout))
/// and per call ([`CallOptions::timeout`]).
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// What a unit gets to know about the call it is handling.
///
/// Handed to every service handler: the unit it is running as and the
/// caller's pid (self-claims are the caller's own problem; the caller's
/// honesty is the cooperative-trust story above). Handlers that need to
/// call back into the tree can capture an [`Init`] clone at build time;
/// this context doesn't duplicate it.
#[derive(Clone, Debug)]
pub struct ServiceContext {
    unit: Pid,
    caller: Pid,
}

impl ServiceContext {
    pub(crate) fn new(unit: Pid, caller: Pid) -> Self {
        Self { unit, caller }
    }

    /// The pid of the unit whose service loop is running.
    #[must_use]
    pub fn unit(&self) -> Pid {
        self.unit
    }

    /// The pid of the unit that made the call.
    #[must_use]
    pub fn caller(&self) -> Pid {
        self.caller
    }
}

/// The reply half of the envelope protocol.
///
/// Public to the crate only: callers never name it, the wiring produces it.
pub(crate) type CallReply = Result<Box<dyn Any + Send>, RouteFailure>;

/// Failure known to the callee side of the envelope protocol.
#[derive(Debug)]
pub(crate) enum RouteFailure {
    /// The payload did not downcast to the handler's request type.
    TypeMismatch,
}

/// A message in flight inside the tree: the request payload, who sent it,
/// and where the answer goes.
///
/// The payload is type-erased; the wiring that owns the handler is the only
/// place that knows the true request type. Downcast failures are a
/// [`RouteFailure::TypeMismatch`], never a panic.
pub(crate) struct Envelope {
    pub(crate) caller: Pid,
    pub(crate) payload: Box<dyn Any + Send>,
    pub(crate) reply: oneshot::Sender<CallReply>,
}

/// Options for one call.
///
/// `Default` means "everything from the spec" (per-unit default, then the
/// global default); `CallOptions::timeout` overrides the deadline for one
/// call, e.g. a health probe that may wait longer than 5s or less.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallOptions {
    /// Deadline for the whole round trip (mailbox hand-off plus handler
    /// run time). Overrides the per-unit and global defaults.
    pub timeout: Option<Duration>,
}

impl CallOptions {
    /// Options carrying an explicit per-call timeout.
    #[must_use]
    pub fn timeout(timeout: Duration) -> Self {
        Self {
            timeout: Some(timeout),
        }
    }
}

/// The error vocabulary for unit-to-unit calls.
///
/// Deliberately small and bounded; the `outcome` metric label carries the
/// same vocabulary ([`CallError::outcome_label`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum CallError {
    /// The round trip exceeded the deadline (per-call option, per-unit
    /// default or the 5s global default).
    #[error("call timed out after {after:?}")]
    Timeout {
        /// The deadline that was in effect.
        after: Duration,
    },

    /// The target's mailbox was full; the call was rejected before it ran.
    #[error("target's mailbox is full")]
    Busy,

    /// The target does not exist, is not running (zombie, restarting,
    /// failed-terminal, reaped), or has no live receiver — plain units
    /// included.
    #[error("target unit is not running")]
    NotRunning,

    /// The capability check failed: the caller is neither an ancestor of
    /// the target, nor the target itself, nor holds a grant to it.
    #[error("caller holds no capability for the target")]
    AccessDenied,

    /// The request or the response did not downcast to the expected types.
    /// The metric records it; nobody panics.
    #[error("request/response type mismatch")]
    TypeMismatch,

    /// The reply channel closed without an answer: the handler panicked
    /// (contained by the task service) or the unit was stopped mid-call.
    #[error("target went away mid-call")]
    Panicked,
}

impl CallError {
    /// The bounded `outcome` label for this failure (or for success, see
    /// [`OUTCOME_OK`]).
    #[must_use]
    pub fn outcome_label(&self) -> &'static str {
        match self {
            Self::Timeout { .. } => OUTCOME_TIMEOUT,
            Self::Busy => OUTCOME_BUSY,
            Self::NotRunning => OUTCOME_NOT_RUNNING,
            Self::AccessDenied => OUTCOME_NOT_CAPPABLE,
            Self::TypeMismatch => OUTCOME_TYPE_MISMATCH,
            Self::Panicked => OUTCOME_PANICKED,
        }
    }
}

/// `ok` for the `outcome` label of a successful call.
pub const OUTCOME_OK: &str = "ok";
/// `timeout` for the `outcome` label.
pub const OUTCOME_TIMEOUT: &str = "timeout";
/// `busy` for the `outcome` label.
pub const OUTCOME_BUSY: &str = "busy";
/// `not_running` for the `outcome` label.
pub const OUTCOME_NOT_RUNNING: &str = "not_running";
/// `panicked` for the `outcome` label.
pub const OUTCOME_PANICKED: &str = "panicked";
/// `type_mismatch` for the `outcome` label.
pub const OUTCOME_TYPE_MISMATCH: &str = "type_mismatch";
/// `not_cappable` for the `outcome` label.
pub const OUTCOME_NOT_CAPPABLE: &str = "not_cappable";

/// The type-erased service wiring stored on the node.
///
/// Built per unit by [`UnitSpec::with_service`](crate::UnitSpec::with_service);
/// `build` is called once per body run (spawn and each restart), so the
/// mailbox is always fresh and the previous run's sender can never alias the
/// current one.
pub(crate) trait ServiceWiring: Send + Sync {
    /// Build the run's mailbox and body. `pid` is the unit the body will
    /// run as; `capacity` is its resolved mailbox capacity.
    fn build(&self, pid: Pid, capacity: usize) -> (mpsc::Sender<Envelope>, UnitBody);
}

/// The wiring behind [`UnitSpec::with_service`](crate::UnitSpec::with_service).
///
/// `H` is stored behind an `Arc` so a restart can clone the handler cheaply;
/// the future type is erased at the wiring boundary.
pub(crate) struct FnServiceWiring<Req, Resp, H> {
    handler: Arc<H>,
    _marker: PhantomData<fn(Req) -> Resp>,
}

impl<Req, Resp, H> FnServiceWiring<Req, Resp, H> {
    pub(crate) fn new(handler: H) -> Self {
        Self {
            handler: Arc::new(handler),
            _marker: PhantomData,
        }
    }
}

impl<Req, Resp, H, Fut> ServiceWiring for FnServiceWiring<Req, Resp, H>
where
    Req: Send + 'static,
    Resp: Send + 'static,
    H: Fn(ServiceContext, Req) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Resp> + Send + 'static,
{
    fn build(&self, pid: Pid, capacity: usize) -> (mpsc::Sender<Envelope>, UnitBody) {
        let (tx, rx) = mpsc::channel(capacity);
        let handler = Arc::clone(&self.handler);
        let body: UnitBody = Box::pin(async move { serve_loop(pid, rx, handler).await });
        (tx, body)
    }
}

/// The service body: receive envelopes, run the handler, answer.
///
/// Exits when every sender is gone (unit removal) or when the body is
/// dropped at an await point (cancel). A handler panic unwinds through here
/// into the task service, which contains it; the caller sees the reply
/// channel close and maps it to [`CallError::Panicked`].
async fn serve_loop<Req, Resp, H, Fut>(
    unit: Pid,
    mut rx: mpsc::Receiver<Envelope>,
    handler: Arc<H>,
) -> ExitStatus
where
    Req: Send + 'static,
    Resp: Send + 'static,
    H: Fn(ServiceContext, Req) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Resp> + Send + 'static,
{
    while let Some(envelope) = rx.recv().await {
        match envelope.payload.downcast::<Req>() {
            Ok(msg) => {
                let ctx = ServiceContext::new(unit, envelope.caller);
                let response = handler(ctx, *msg).await;
                let _ = envelope
                    .reply
                    .send(Ok(Box::new(response) as Box<dyn Any + Send>));
            }
            Err(_) => {
                let _ = envelope.reply.send(Err(RouteFailure::TypeMismatch));
            }
        }
    }
    ExitStatus::Success
}

tokio::task_local! {
    /// The pid of the unit whose body is currently running.
    pub(crate) static CURRENT_UNIT: Pid;
}

/// The pid of the unit whose body is currently running, if there is one.
///
/// Works inside unit bodies (task-local), including service handlers, which
/// additionally get [`ServiceContext`] as their first argument. Returns
/// `None` anywhere else (tests, the composition root before boot) — calls
/// made there need an explicit caller pid.
#[must_use]
pub fn current_unit() -> Option<Pid> {
    CURRENT_UNIT.try_with(|pid| *pid).ok()
}

/// Resolved view of a target node for one call.
pub(crate) struct ResolvedTarget {
    pub(crate) sender: mpsc::Sender<Envelope>,
    pub(crate) caller_default_timeout: Duration,
}

/// Resolve the target and check the capability.
///
/// Returns the target's bounded label alongside failure reasons so the
/// caller can record the metric on every path. One critical section, no
/// await, no per-call allocation beyond the sender clone.
pub(crate) fn resolve_call(
    st: &TreeState,
    caller: Pid,
    target: Pid,
) -> (&'static str, Result<ResolvedTarget, CallError>) {
    // the caller's default timeout rides along even on the error path: it is
    // looked up in the same lock either way
    let caller_default_timeout = st
        .get_node(caller)
        .and_then(|view| view.call_timeout)
        .unwrap_or(DEFAULT_CALL_TIMEOUT);
    let Some(view) = st.get_node(target) else {
        return (ServiceKind::Unit.as_str(), Err(CallError::NotRunning));
    };
    let kind = view.service_kind.unwrap_or(ServiceKind::Unit).as_str();
    if view.state != UnitState::Running {
        return (kind, Err(CallError::NotRunning));
    }
    let allowed =
        caller == target || st.is_ancestor(caller, target) || st.has_grant(caller, target);
    if !allowed {
        return (kind, Err(CallError::AccessDenied));
    }
    let Some(sender) = view.mailbox else {
        return (kind, Err(CallError::NotRunning));
    };
    (
        kind,
        Ok(ResolvedTarget {
            sender,
            caller_default_timeout,
        }),
    )
}

/// Emit the one-counter / one-histogram pair and the one tracing event.
///
/// `target` is the bounded service-kind vocabulary, never a pid or a unit
/// name; `outcome` is from the bounded set above. Payload values are never
/// logged.
pub(crate) fn record_ipc(
    target: &'static str,
    outcome: &'static str,
    started: Instant,
    caller: Pid,
    target_pid: Pid,
) {
    crate::metrics::record_call(target, outcome);
    crate::metrics::record_duration(target, outcome, started.elapsed());
    if outcome == OUTCOME_OK {
        tracing::debug!(%caller, %target_pid, "ipc call");
    } else {
        tracing::warn!(%caller, %target_pid, outcome, "ipc call failed");
    }
}

impl Init {
    /// Call another unit in the tree.
    ///
    /// `caller` is the unit the call is attributed to; use
    /// [`Init::call_current`] inside unit bodies to attribute it to the
    /// body's own unit. The capability check is ancestor-of-target or a
    /// grant installed at spawn (see the module docs). The request is
    /// type-erased into the mailbox and downcast on the callee; a mismatch
    /// is [`CallError::TypeMismatch`], never a panic.
    ///
    /// # Cancellation
    ///
    /// If the target is stopped while answering, the call fails with
    /// [`CallError::Panicked`] (the reply channel closes). If the caller is
    /// cancelled while waiting, this future is dropped and no answer is
    /// awaited.
    ///
    /// # Errors
    ///
    /// [`CallError`] — `Timeout`, `Busy`, `NotRunning`, `AccessDenied`,
    /// `TypeMismatch` or `Panicked`.
    pub async fn call<Req, Resp>(
        &self,
        caller: Pid,
        target: Pid,
        msg: Req,
    ) -> Result<Resp, CallError>
    where
        Req: Send + 'static,
        Resp: Send + 'static,
    {
        self.call_with_options(caller, target, msg, CallOptions::default())
            .await
    }

    /// Like [`Init::call`], with per-call options.
    ///
    /// Resolution order for the deadline: `options.timeout`, then the
    /// caller's [`UnitSpec::with_call_timeout`](crate::UnitSpec::with_call_timeout),
    /// then [`DEFAULT_CALL_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// Same vocabulary as [`Init::call`].
    pub async fn call_with_options<Req, Resp>(
        &self,
        caller: Pid,
        target: Pid,
        msg: Req,
        options: CallOptions,
    ) -> Result<Resp, CallError>
    where
        Req: Send + 'static,
        Resp: Send + 'static,
    {
        let started = Instant::now();
        // phase 1: one short critical section — resolve, authorize, clone the
        // sender. The registry lock is dropped before any await or send.
        let (kind, resolved) = {
            let st = self.lock_state();
            resolve_call(&st, caller, target)
        };
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(err) => {
                record_ipc(kind, err.outcome_label(), started, caller, target);
                return Err(err);
            }
        };
        // phase 2: hand off. try_send is non-blocking; no lock is held.
        let timeout = options.timeout.unwrap_or(resolved.caller_default_timeout);
        let (reply_tx, reply_rx) = oneshot::channel();
        let envelope = Envelope {
            caller,
            payload: Box::new(msg),
            reply: reply_tx,
        };
        match resolved.sender.try_send(envelope) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                record_ipc(kind, OUTCOME_BUSY, started, caller, target);
                return Err(CallError::Busy);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                record_ipc(kind, OUTCOME_NOT_RUNNING, started, caller, target);
                return Err(CallError::NotRunning);
            }
        }
        // phase 3: wait for the answer, bounded by the deadline. The registry
        // lock is not held while waiting; the deadlock lottery test in
        // tests/ipc.rs enforces this constructively.
        let result = match tokio::time::timeout(timeout, reply_rx).await {
            Err(_) => Err(CallError::Timeout { after: timeout }),
            Ok(Err(_closed)) => Err(CallError::Panicked),
            Ok(Ok(Ok(boxed))) => match boxed.downcast::<Resp>() {
                Ok(resp) => Ok(*resp),
                Err(_) => Err(CallError::TypeMismatch),
            },
            Ok(Ok(Err(RouteFailure::TypeMismatch))) => Err(CallError::TypeMismatch),
        };
        let outcome = match &result {
            Ok(_) => OUTCOME_OK,
            Err(err) => err.outcome_label(),
        };
        record_ipc(kind, outcome, started, caller, target);
        result
    }

    /// Call another unit, attributed to the unit whose body is executing.
    ///
    /// Inside a unit body this resolves the caller from a task-local set by
    /// the runner; outside one there is nothing to attribute the call to and
    /// [`CallError::AccessDenied`] is returned (with the `not_cappable`
    /// metric). Deadline resolution matches [`Init::call_with_options`] with
    /// default options.
    ///
    /// # Errors
    ///
    /// Same vocabulary as [`Init::call`], plus `AccessDenied` when called
    /// outside a unit body.
    pub async fn call_current<Req, Resp>(&self, target: Pid, msg: Req) -> Result<Resp, CallError>
    where
        Req: Send + 'static,
        Resp: Send + 'static,
    {
        let started = Instant::now();
        let Some(caller) = current_unit() else {
            record_ipc(
                ServiceKind::Unit.as_str(),
                OUTCOME_NOT_CAPPABLE,
                started,
                Pid::NONE,
                target,
            );
            return Err(CallError::AccessDenied);
        };
        self.call(caller, target, msg).await
    }

    /// The pids this unit holds grants to, sorted ascending.
    ///
    /// Inspection hook for operator tooling and tests (`/candyriya ps` will
    /// consume it in phase 5). Empty for units without grants.
    #[must_use]
    pub fn grants(&self, pid: Pid) -> Vec<Pid> {
        self.lock_state().grants_of(pid)
    }
}

/// An owned, resolved snapshot of a node's call-relevant state.
///
/// Crate-internal read model; the registry stays the source of truth.
pub(crate) struct NodeCallView {
    /// Lifecycle state at read time.
    pub(crate) state: UnitState,
    /// The mailbox sender, if the node still holds one.
    pub(crate) mailbox: Option<mpsc::Sender<Envelope>>,
    /// The service-kind label.
    pub(crate) service_kind: Option<ServiceKind>,
    /// The per-unit default timeout for outbound calls.
    pub(crate) call_timeout: Option<Duration>,
}

/// Index of granted capabilities, mirrored both ways so reaps prune in
/// O(grants of one unit) instead of scanning the whole table.
#[derive(Default)]
pub(crate) struct Grants {
    /// holder -> granted targets
    by_holder: HashMap<Pid, std::collections::HashSet<Pid>>,
    /// target -> holders (for pruning on reap)
    by_target: HashMap<Pid, std::collections::HashSet<Pid>>,
}

impl Grants {
    pub(crate) fn insert(&mut self, holder: Pid, target: Pid) {
        self.by_holder.entry(holder).or_default().insert(target);
        self.by_target.entry(target).or_default().insert(holder);
    }

    pub(crate) fn contains(&self, holder: Pid, target: Pid) -> bool {
        self.by_holder
            .get(&holder)
            .is_some_and(|targets| targets.contains(&target))
    }

    pub(crate) fn grants_of(&self, holder: Pid) -> Vec<Pid> {
        let mut targets: Vec<Pid> = self
            .by_holder
            .get(&holder)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default();
        targets.sort();
        targets
    }

    /// Drop every grant that involves `pid`, in both directions.
    pub(crate) fn prune_involving(&mut self, pid: Pid) {
        if let Some(targets) = self.by_holder.remove(&pid) {
            for target in targets {
                if let Some(holders) = self.by_target.get_mut(&target) {
                    holders.remove(&pid);
                    if holders.is_empty() {
                        self.by_target.remove(&target);
                    }
                }
            }
        }
        if let Some(holders) = self.by_target.remove(&pid) {
            for holder in holders {
                if let Some(targets) = self.by_holder.get_mut(&holder) {
                    targets.remove(&pid);
                    if targets.is_empty() {
                        self.by_holder.remove(&holder);
                    }
                }
            }
        }
    }
}
