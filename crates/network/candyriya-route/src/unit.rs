//! The `route` unit: the supervised owner of the routing registry.
//!
//! Every runtime routing read — which backends may serve this host, which
//! health state each backend is in — goes through this unit as a typed IPC
//! call; no subsystem reads the registry directly once the tree is up. The
//! composition root spawns the unit with the shared
//! [`ArcSwap`] object holding the current `Option<Arc<BackendRegistry>>`,
//! and the same object is what `/candyriya reload` swaps through
//! (`ProxyState::set_router`) — one object, so a reload is visible to the
//! next call without any re-registration.
//!
//! # Why the protocol lives here
//!
//! The message pair is the contract between the composition root (which
//! spawns the unit and attaches the handler) and its callers (the proxy and
//! the CLI). It is deliberately small: two request/reply enums, zero
//! serialization — the tree type-erases envelopes and downcasts on the
//! callee. This is internal wiring, not a stabilized public API; adding a
//! variant is an explicit, reviewable change.
//!
//! # Reply cost
//!
//! Replies carry owned data, never a reference into the registry: names are
//! cloned `String`s, backends cross as [`BackendRef`] (name + address), and
//! [`BackendSnapshot`] carries name + state. Nothing here borrows from the
//! registry, so a caller never pins an old registry version by accident.
//!
//! # Capability
//!
//! Units call [`Init::call`] with their own pid; the subtree rule alone
//! does not cover sibling calls, so a unit that reads routing needs a grant
//! to this unit, installed at its spawn (the composition root grants one to
//! `stats.every-60s`). Code outside unit bodies attributes to
//! [`Pid::INIT`], which is an ancestor of every unit, so the subtree rule
//! authorizes the call; per-connection tasks use that path today.
//!
//! # Failure vocabulary
//!
//! Callers see the tree's [`CallError`](candyriya_init::CallError). A reply
//! is only ever produced by a running unit; a missing or crashed unit fails
//! fast with `NotRunning`, a live-but-slow one with `Timeout`. The unit is
//! spawned with [`RestartPolicy::Always`] — it is the source of truth for
//! routing — but the supervision window in between can refuse a call; every
//! call site decides for itself how to fail.
//!
//! # Health checking
//!
//! [`health_spec`] describes the child unit `route.health`: it re-checks
//! every backend every [`DEFAULT_HEALTH_CHECK_INTERVAL`] (30s, a
//! code-side default — there is no config key today; adding one belongs to
//! the reload phase) using the same shared store. The check is a full
//! status ping per backend, fully async, never blocking a worker. The
//! health unit owns the loop; the registry owns the mechanics. Outcomes are
//! logged by [`BackendRegistry::health_check_with_timeout`] itself, and the
//! unit records the number of online backends in the `candyriya_backends_online`
//! gauge after each round.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tracing::debug;

use candyriya_init::{CallError, Init, Pid, RestartPolicy, ServiceKind, UnitKind, UnitSpec};

use crate::backend::BackendState;
use crate::registry::{BackendRegistry, DEFAULT_HEALTH_CHECK_TIMEOUT};

/// How often the `route.health` child unit re-checks every backend.
///
/// No config key today by design (the health loop is new; its tuning is
/// expected to land with the reload phase, which owns routing changes). 30
/// seconds matches the scale at which operators care about a backend
/// disappearing; the check itself is sub-second per backend and sequential.
pub const DEFAULT_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// The shared store holding the current routing registry.
///
/// This is the one object the reload path swaps through
/// (`ProxyState::set_router`) and the one the route unit serves from —
/// sharing it is what keeps `/candyriya reload` working unchanged. The
/// `Option` inside is honest about routing being disabled: `None` means no
/// `[servers]` configured (or routing disabled by a reload), and every
/// message answers with its documented empty reply.
pub type RouterStore = Arc<ArcSwap<Option<Arc<BackendRegistry>>>>;

/// Build a shared store holding `router`.
///
/// A convenience for call sites (composition root, tests, fixtures) so the
/// `ArcSwap` mechanics stay inside this crate.
#[must_use]
pub fn router_store(router: Option<Arc<BackendRegistry>>) -> RouterStore {
    Arc::new(ArcSwap::from_pointee(router))
}

/// Messages the `route` unit answers.
///
/// One enum per service, per the tree's IPC contract. Variants mirror what
/// the proxy does directly today; anything else is a new variant by explicit
/// change.
#[derive(Debug)]
pub enum RouteMsg {
    /// Forced-hosts lookup for `host`, with the same matching semantics as
    /// [`crate::ForcedHosts`] (normalization, single-label wildcard,
    /// declaration order). The reply is the dial-able set, in declaration
    /// order.
    Resolve {
        /// The host the client connected to.
        host: String,
    },
    /// The ordered try list, resolved to dial-able backends.
    TryChain,
    /// The last observed health state of `name`; `None` when the name is
    /// not registered (or routing is disabled).
    State {
        /// The backend name to query.
        name: String,
    },
    /// Overwrite the health state of `name` (the login path's
    /// mark-backend-offline flow).
    SetState {
        /// The backend name to mark.
        name: String,
        /// The state to record.
        state: BackendState,
    },
    /// Every backend name, in registration order.
    Names,
    /// Name and state of every backend, in registration order.
    Snapshot,
}

/// Replies from the `route` unit.
///
/// Owned data only — nothing borrows from the store.
#[derive(Debug)]
pub enum RouteResp {
    /// The forced-hosts match for the host, as dial-able backends.
    /// `None` when no pattern matches (or routing is disabled); the caller
    /// falls back to the try chain, exactly as it did before the unit
    /// existed. An empty list means the match itself is unreachable; it
    /// falls back the same way.
    Resolved(Option<Vec<BackendRef>>),
    /// The try list, as dial-able backends, in order. Empty when routing is
    /// disabled.
    Chain(Vec<BackendRef>),
    /// The last observed health state of the requested backend.
    State(Option<BackendState>),
    /// `true` when the name is registered and was marked; `false` for an
    /// unknown name (or disabled routing). Never a hard error — the caller
    /// treats it as best effort.
    SetState(bool),
    /// Every backend name, in registration order. Empty when routing is
    /// disabled.
    Names(Vec<String>),
    /// Name and state of every backend, in registration order. Empty when
    /// routing is disabled.
    Snapshot(Vec<BackendSnapshot>),
}

/// One dial-able backend crossing the IPC boundary.
///
/// Created fresh for every reply that needs it; never tied to the registry's
/// lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRef {
    name: String,
    addr: SocketAddr,
}

impl BackendRef {
    /// The backend's configured name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The socket address the proxy dials for this backend.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

/// One backend's name and health state, for callers that need a table
/// rather than a [`BackendRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSnapshot {
    name: String,
    state: BackendState,
}

impl BackendSnapshot {
    /// The backend's configured name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The last observed health state.
    #[must_use]
    pub fn state(&self) -> BackendState {
        self.state
    }
}

/// Handle one message against the registry held by `store`.
///
/// Pure with respect to the tree: no IPC happens here, the caller
/// ([`route_spec`]) is the transport. Split out so the semantics are
/// documented next to the registry they operate on.
fn current(store: &RouterStore) -> Option<Arc<BackendRegistry>> {
    (*store.load_full()).clone()
}

fn handle_msg(store: &RouterStore, msg: RouteMsg) -> RouteResp {
    match msg {
        RouteMsg::Resolve { host } => {
            let Some(registry) = current(store) else {
                return RouteResp::Resolved(None);
            };
            match registry.resolve_host(&host) {
                Some(names) => RouteResp::Resolved(Some(
                    names
                        .iter()
                        .filter_map(|name| registry.get(name))
                        .map(backend_ref)
                        .collect(),
                )),
                None => RouteResp::Resolved(None),
            }
        }
        RouteMsg::TryChain => {
            let Some(registry) = current(store) else {
                return RouteResp::Chain(Vec::new());
            };
            RouteResp::Chain(
                registry
                    .try_chain()
                    .iter()
                    .filter_map(|name| registry.get(name))
                    .map(backend_ref)
                    .collect(),
            )
        }
        RouteMsg::State { name } => {
            let Some(registry) = current(store) else {
                return RouteResp::State(None);
            };
            RouteResp::State(registry.state(&name))
        }
        RouteMsg::SetState { name, state } => {
            let Some(registry) = current(store) else {
                return RouteResp::SetState(false);
            };
            RouteResp::SetState(registry.set_state(&name, state).is_ok())
        }
        RouteMsg::Names => {
            let Some(registry) = current(store) else {
                return RouteResp::Names(Vec::new());
            };
            RouteResp::Names(
                registry
                    .iter()
                    .map(|backend| backend.name().to_owned())
                    .collect(),
            )
        }
        RouteMsg::Snapshot => {
            let Some(registry) = current(store) else {
                return RouteResp::Snapshot(Vec::new());
            };
            RouteResp::Snapshot(
                registry
                    .iter()
                    .map(|backend| BackendSnapshot {
                        name: backend.name().to_owned(),
                        state: backend.state(),
                    })
                    .collect(),
            )
        }
    }
}

fn backend_ref(backend: &crate::backend::Backend) -> BackendRef {
    BackendRef {
        name: backend.name().to_owned(),
        addr: backend.addr(),
    }
}

/// Describe the `route` unit for spawning.
///
/// The composition root (or a test) hands the shared store in; the unit
/// owns one clone and serves it until the process exits or the tree restarts
/// it. Always spawned, even with no `[servers]` configured: an empty store
/// is an honest state — every message answers its documented empty reply,
/// and the login path disconnects clients with the generic no-backend
/// kick. A `/candyriya reload` that later enables routing swaps the same
/// store, so the next call sees it.
///
/// The unit runs `LongLived` under an `Always` policy: routing is the
/// source of truth for how players reach backends, so the supervisor
/// replaces the unit instead of leaving callers to degrade.
#[must_use]
pub fn route_spec(store: RouterStore) -> UnitSpec {
    UnitSpec::service(
        "route",
        UnitKind::LongLived,
        RestartPolicy::Always,
        move |_ctx, msg: RouteMsg| {
            let store = Arc::clone(&store);
            async move { handle_msg(&store, msg) }
        },
    )
    .with_service_kind(ServiceKind::Router)
}

/// Describe the `route.health` child unit for spawning.
///
/// The unit is meant to run as a child of the route unit (spawn it with the
/// route unit's pid as parent): it re-checks every backend every
/// `interval`, loading the same shared store and calling
/// [`BackendRegistry::health_check_with_timeout`]. Checks are a full status
/// ping each, run fully async and sequentially — never blocking a worker,
/// matching the registry's own contract.
///
/// This is the first production caller of the health check; it closes the
/// gap where backend health was never observed outside tests. Outcomes are
/// logged by the registry itself (per-backend `debug`/`warn`); each round
/// records `candyriya_backends_online` as a gauge and emits a single debug
/// line. Per-backend state is observable through the unit's IPC replies.
///
/// If `[servers]` is empty at boot this unit is not spawned at all (nothing
/// to check); if a `/candyriya reload` later enables routing, nothing
/// health-checks the new backends until the next restart — wiring health
/// into the reload path is deliberately out of scope here.
///
/// The unit runs `Every` with an `OnFailure` policy: the loop is the body,
/// and a crash restarts it with the tree's backoff. The loop never returns;
/// a cancel drops the body mid-sleep like any other unit.
#[must_use]
pub fn health_spec(store: RouterStore, interval: Duration) -> UnitSpec {
    UnitSpec::new(
        "route.health",
        UnitKind::Every,
        RestartPolicy::OnFailure,
        move || {
            let store = Arc::clone(&store);
            async move {
                loop {
                    let Some(registry) = current(&store) else {
                        debug!("routing is disabled; skipping this backend health round");
                        tokio::time::sleep(interval).await;
                        continue;
                    };
                    let outcomes = registry
                        .health_check_with_timeout(DEFAULT_HEALTH_CHECK_TIMEOUT)
                        .await;
                    let online = outcomes.iter().filter(|outcome| outcome.healthy()).count();
                    metrics::gauge!(candyriya_metrics::names::NAME_BACKENDS_ONLINE)
                        .set(online as f64);
                    debug!(
                        online,
                        total = outcomes.len(),
                        "backend health check round complete"
                    );
                    tokio::time::sleep(interval).await;
                }
            }
        },
    )
    .with_service_kind(ServiceKind::Router)
}

/// Call the `route` unit with `msg`.
///
/// `caller` is the unit the call is attributed to. Inside a unit body the
/// caller is the body's own unit, and the unit must hold a grant to the
/// route unit — the grant is installed at spawn, exactly like an inherited
/// fd (the composition root grants one to `stats.every-60s`). Code outside a
/// unit body attributes to [`Pid::INIT`], which is an ancestor of every
/// unit, so the subtree rule authorizes the call; per-connection tasks use
/// that path today.
///
/// # Errors
///
/// Whatever the tree reports: `NotRunning` (the unit is down), `Busy` (its
/// mailbox is full), `Timeout`, `Panicked`, `AccessDenied` (the caller is a
/// sibling unit without the grant) or `TypeMismatch`.
pub async fn route_call(
    init: &Init,
    caller: Pid,
    target: Pid,
    msg: RouteMsg,
) -> Result<RouteResp, CallError> {
    init.call(caller, target, msg).await
}
