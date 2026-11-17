//! The accept loop, the shared proxy state and the per-connection task.
//!
//! [`serve`] registers the loop as the named task `proxy.accept-loop`
//! on the [`candyriya_scheduler::TaskService`]; connections are
//! [`TaskService::spawn_named`]d as `proxy.conn.<id>`. There is no
//! ad-hoc `tokio::spawn` anywhere in this module — the service is the
//! only place tasks are born, watching, getting cancelled and drained.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tracing::{debug, error, info, warn};

use candyriya_auth::AuthConfig;
use candyriya_config::Config;
use candyriya_init::{CallError, Init, Pid, current_unit};
use candyriya_plugin_api::{EventDispatcher, EventIdRegistry};
use candyriya_ratelimit::ConnectionLimiter;
use candyriya_route::{BackendRef, BackendRegistry, RouteMsg, RouteResp, RouterStore, route_call};
use candyriya_scheduler::{TaskId, TaskService};

use crate::config_unit;
use crate::handshake::{HandshakeLimits, read_handshake};
use crate::limit::{ConnectionSlot, SlotAcquireError, try_acquire_slot};
use crate::roster::PlayerRoster;
use crate::status::{FaviconCache, handle_status};
use candyriya_protocol::Intent;

/// How long to pause the loop after a failed `accept(2)`.
///
/// `accept` failures are almost always kernel-side exhaustion
/// (`EMFILE`, `ENFILE`, ...); spinning would hammer the kernel for a
/// resource it does not have, so the loop backs off briefly instead.
const ACCEPT_FAILURE_BACKOFF: Duration = Duration::from_millis(50);

/// State shared between the accept loop and the connection tasks.
///
/// Deliberately minimal: a monotonic connection counter, the connection
/// limiter and the handles the composition root wired in. The configuration
/// is deliberately NOT part of it: no subsystem reads the store directly,
/// every read goes to the `config` unit through the tree (the reply carries
/// the same snapshot the store holds, so readers are as cheap as a refcount
/// bump plus a mailbox round trip). The routing registry is not part of it
/// either: every routing read goes to the `route` unit through the tree;
/// the only exception is the atomic swap the reload path performs through
/// [`ProxyState::set_router`].
pub struct ProxyState {
    /// How to reach the supervision tree; cloned freely.
    init: Init,
    /// The `config` unit's pid, granted at spawn (the root is an ancestor of
    /// it, so the subtree rule authorizes the calls without a grant).
    config_pid: Pid,
    /// The `route` unit's pid. Like the config pid, sibling units would need
    /// a grant to reach it; the root and per-connection tasks do not.
    route_pid: Pid,
    /// Monotonic connection-id counter. Starts at 0; handed out ids
    /// start at 1.
    pub conn_counter: AtomicU64,
    /// Concurrent-connection accounting: per-IP cap plus (unbounded)
    /// limiter-side global accounting. The effective global cap is the
    /// semaphore the accept loop creates from `[advanced]
    /// max-connections`.
    pub limiter: ConnectionLimiter,
    /// Optional binary favicon cache (cachegirl). `None` by default; the
    /// composition root wires it when it owns a `CacheManager`. Without
    /// one, favicons are read from disk per ping.
    pub favicon_cache: Option<Arc<FaviconCache>>,
    /// Authentication endpoint configuration: the session-server URL and
    /// timeout used for the online-mode `hasJoined` verify. Ignored in
    /// offline mode. Only meaningful when `[online-mode]` is true.
    pub auth: AuthConfig,
    /// The shared store holding the routing registry. Owned by the `route`
    /// unit, swapped by the reload path through [`ProxyState::set_router`];
    /// this handle exists only so the swap stays on the same object the unit
    /// serves from — it is never read here. `None` in the option means
    /// routing is disabled (no `[servers]`); the unit answers its empty
    /// replies in that state. Login-time readers go to the unit.
    router_store: RouterStore,
    /// Connections refused before they were served (per-IP cap or the
    /// global `max-connections` semaphore). Read by the composition root
    /// for the status snapshot and metrics.
    pub connection_rejections: AtomicU64,
    /// Players currently in the play state, with the backend they landed
    /// on. Populated on the transition to play, drained when the byte pump
    /// ends; the source of truth for `glist` and the player counters.
    pub roster: PlayerRoster,
    /// The shared plugin event dispatcher. `None` until the composition
    /// root wires one in at boot — without it the proxy fires no events
    /// and behaves exactly as if plugins did not exist.
    pub event_dispatcher: Option<Arc<dyn EventDispatcher>>,
    /// Localized kick text for plugin-vetoed actions. Built by the
    /// composition root from the configured locale (embedded locale plus
    /// the `locales/` override directory). `None` in tests and embedded
    /// tooling; the kick falls back to the embedded English string then.
    pub event_i18n: Option<Arc<candyriya_i18n::I18n>>,
    /// Monotonic ids for the events the proxy fires (the dispatch ABI
    /// requires envelope ids to increase by one per event).
    pub(crate) event_ids: EventIdRegistry,
}

impl core::fmt::Debug for ProxyState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ProxyState")
            .field("config_pid", &self.config_pid)
            .field("route_pid", &self.route_pid)
            .field("conn_counter", &self.conn_counter.load(Ordering::Relaxed))
            .field(
                "connection_rejections",
                &self.connection_rejections.load(Ordering::Relaxed),
            )
            .field("players_online", &self.roster.len())
            .field("current_connections", &self.limiter.current_connections())
            .finish_non_exhaustive()
    }
}

impl ProxyState {
    /// State from the tree alone: the proxy learns its configuration from
    /// the `config` unit and its routing from the `route` unit, not from
    /// anything it was handed at construction.
    ///
    /// The per-IP cap falls back to [`crate::DEFAULT_PER_IP_MAX`]
    /// because `[advanced]` has no per-IP key yet.
    ///
    /// `router_store` is the shared store the `route` unit serves from
    /// (created by the caller with [`candyriya_route::router_store`] and
    /// handed to [`candyriya_route::route_spec`] as well); this state keeps
    /// one handle so the reload path can swap the registry through it. The
    /// option inside the store starts as the caller seeded it.
    pub fn new(init: Init, config_pid: Pid, route_pid: Pid, router_store: RouterStore) -> Self {
        Self::with_limits(
            init,
            config_pid,
            route_pid,
            router_store,
            crate::DEFAULT_PER_IP_MAX,
        )
    }

    /// State with an explicit per-IP cap.
    ///
    /// Split from [`ProxyState::new`] so tests (and, later, a config
    /// key) can pin the per-IP cap without mutating the config crate.
    pub fn with_limits(
        init: Init,
        config_pid: Pid,
        route_pid: Pid,
        router_store: RouterStore,
        per_ip_max: u64,
    ) -> Self {
        // The limiter's own global cap is unbounded by design: the
        // semaphore in the accept loop is the single owner of the
        // global cap (see the `limit` module docs).
        let limiter = ConnectionLimiter::with_per_ip_max(u64::MAX, per_ip_max);
        Self {
            init,
            config_pid,
            route_pid,
            conn_counter: AtomicU64::new(0),
            limiter,
            favicon_cache: None,
            auth: AuthConfig::default(),
            router_store,
            connection_rejections: AtomicU64::new(0),
            roster: PlayerRoster::new(),
            event_dispatcher: None,
            event_i18n: None,
            event_ids: EventIdRegistry::new(),
        }
    }

    /// State with a backend router pre-wired (the composition root's job;
    /// tests wire their own fixtures).
    ///
    /// The swap goes through the shared store, so the `route` unit serves
    /// the fresh registry from this moment on.
    pub fn with_router(self, router: Arc<BackendRegistry>) -> Self {
        self.set_router(Some(router));
        self
    }

    /// Atomically replace the routing registry.
    ///
    /// Called by the reload path after the backend set is rebuilt from the
    /// new configuration. `None` disables routing entirely (no `[servers]`
    /// configured). The swap happens through the shared store, so the
    /// `route` unit serves the new table to the very next caller — never a
    /// half-swapped route table, and no re-registration of anything.
    pub fn set_router(&self, router: Option<Arc<BackendRegistry>>) {
        self.router_store.store(Arc::new(router));
    }

    /// The current configuration snapshot, fetched from the `config` unit.
    ///
    /// Every runtime read goes through the tree: the reply carries the same
    /// snapshot the store holds right now, so a `/candyriya reload` on the
    /// other end of the process is visible to the next reader, while a
    /// reader keeps the version it started with. In-flight connections keep
    /// the snapshot they fetched; nothing here is cached.
    ///
    /// Callers that cannot proceed honestly without the config (the login
    /// and status paths) turn [`CallError`] into a generic, client-safe
    /// disconnect; callers that cannot fail (the accept loop at start)
    /// refuse to serve. See the individual call sites for the policy.
    ///
    /// # Errors
    ///
    /// [`CallError`] — the unit is down (`NotRunning`), its mailbox is full
    /// (`Busy`), the round trip exceeded the deadline (`Timeout`), or the
    /// handler failed (`Panicked`). No path here falls back to a stale
    /// snapshot.
    ///
    /// # Capability
    ///
    /// Unit bodies attribute the call to themselves and need a spawn-time
    /// grant to the config unit (installed on `proxy.accept` by the
    /// composition root). Code outside a unit body attributes to the root.
    pub async fn config(&self) -> Result<Arc<Config>, CallError> {
        // per-connection tasks are not units yet (phase 3); they attribute
        // to the root, which is an ancestor of every unit, so the subtree
        // rule authorizes the call. Unit bodies attribute to themselves.
        let caller = current_unit().unwrap_or(Pid::INIT);
        config_unit::config(&self.init, caller, self.config_pid).await
    }

    /// Send `msg` to the `route` unit and await its reply.
    ///
    /// Every runtime routing read goes through the tree: forced-hosts
    /// resolution, the try chain, per-backend health state, names and the
    /// snapshot used by the stats line and the directory. The reply is
    /// computed against the shared store, which a `/candyriya reload`
    /// swaps through — the next call sees the new table.
    ///
    /// Callers that cannot proceed honestly without routing (the login
    /// path) turn [`CallError`] into a generic, client-safe disconnect;
    /// callers that cannot fail (a stats tick) log and move on. See the
    /// individual call sites for the policy.
    ///
    /// # Errors
    ///
    /// [`CallError`] — the unit is down (`NotRunning`), its mailbox is full
    /// (`Busy`), the round trip exceeded the deadline (`Timeout`), or the
    /// handler failed (`Panicked`). No path here falls back to a stale
    /// registry.
    ///
    /// # Capability
    ///
    /// Unit bodies attribute the call to themselves and need a spawn-time
    /// grant to the route unit (installed on `stats.every-60s` by the
    /// composition root). Code outside a unit body attributes to the root.
    pub async fn route(&self, msg: RouteMsg) -> Result<RouteResp, CallError> {
        // per-connection tasks are not units yet (phase 3); they attribute
        // to the root, which is an ancestor of every unit, so the subtree
        // rule authorizes the call. Unit bodies attribute to themselves.
        let caller = current_unit().unwrap_or(Pid::INIT);
        route_call(&self.init, caller, self.route_pid, msg).await
    }

    /// The ordered backends to try for `host`, from the `route` unit.
    ///
    /// Forced-host matches win; otherwise the try chain. This is the login
    /// path's routing step, as one pair of IPC calls (forced-hosts probe,
    /// then the chain when there is no match — the same composition that
    /// used to read the registry directly).
    ///
    /// On a route-unit outage the answer is an empty list — the login
    /// flow then applies its generic "no backend available" disconnect.
    /// The error is logged loudly; the client never sees it.
    pub(crate) async fn backends_for(&self, host: &str) -> Vec<BackendRef> {
        match self
            .route(RouteMsg::Resolve {
                host: host.to_owned(),
            })
            .await
        {
            Ok(RouteResp::Resolved(Some(resolved))) if !resolved.is_empty() => return resolved,
            Ok(RouteResp::Resolved(_)) => {}
            Ok(other) => {
                error!(reply = ?other, "unexpected route unit reply to a Resolve call")
            }
            Err(error) => {
                error!(%error, "route unit unavailable during backend resolution");
                return Vec::new();
            }
        }
        match self.route(RouteMsg::TryChain).await {
            Ok(RouteResp::Chain(chain)) => chain,
            Ok(other) => {
                error!(reply = ?other, "unexpected route unit reply to a TryChain call");
                Vec::new()
            }
            Err(error) => {
                error!(%error, "route unit unavailable while fetching the try chain");
                Vec::new()
            }
        }
    }

    /// Report that the backend named `name` failed to connect.
    ///
    /// Best effort by contract (the health state is advisory): failures to
    /// deliver the report are logged and never alter the login flow. The
    /// backend is marked [`BackendState::Offline`](candyriya_route::BackendState::Offline)
    /// in the registry's health accounting, the same way a connect failure
    /// was recorded before the unit existed.
    pub(crate) async fn mark_backend_offline(&self, name: &str) {
        match self
            .route(RouteMsg::SetState {
                name: name.to_owned(),
                state: candyriya_route::BackendState::Offline,
            })
            .await
        {
            Ok(RouteResp::SetState(true)) => {}
            Ok(RouteResp::SetState(false)) => {
                debug!(%name, "health report for an unregistered backend name; ignored")
            }
            Ok(other) => {
                debug!(reply = ?other, "unexpected route unit reply to a SetState call")
            }
            Err(error) => {
                debug!(%error, %name, "failed to report backend connect failure to the route unit")
            }
        }
    }

    /// State with a custom auth endpoint configuration (tests pointing the
    /// session server at a fixture).
    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        self.auth = auth;
        self
    }

    /// Allocate the next connection id.
    fn next_conn_id(&self) -> u64 {
        self.conn_counter.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// The accept loop.
///
/// Accepts connections from `listener` until `shutdown` flips to `true`
/// (or the sending half of the watch channel is dropped), and hands each
/// one to the limit checks and [`handle_connection`].
///
/// The global-connection cap is sized from the `config` unit at loop start.
/// If the unit cannot produce a snapshot, the loop refuses to serve and
/// returns `false`; the caller (the supervision unit hosting the loop, or a
/// test) turns that into a failure — a boot-time configuration outage aborts
/// the boot rather than serving with an invented cap.
///
/// Rejection paths (per-IP cap exhausted, global semaphore exhausted)
/// close the connection immediately and log a warning — a client that
/// cannot be served must not be left hanging.
///
/// The loop applies a short backoff after a failed `accept(2)` so a
/// transient kernel-side condition (e.g. `EMFILE`) cannot spin the
/// worker. Accepted connections keep running in their own tasks after
/// this function returns.
///
/// This is normally driven through [`serve`], which registers the loop
/// on the task service; it is public so tests and the composition root
/// can drive it directly.
///
/// Returns `true` when the loop ran until the shutdown signal, `false`
/// when it refused to serve at startup.
pub async fn accept_loop(
    state: Arc<ProxyState>,
    listener: TcpListener,
    shutdown: watch::Receiver<bool>,
    service: TaskService,
) -> bool {
    let semaphore = Arc::new(Semaphore::new(match state.config().await {
        Ok(config) => usize::try_from(config.advanced.max_connections).unwrap_or(usize::MAX),
        Err(error) => {
            error!(%error, "configuration unavailable at accept loop start; refusing to serve");
            return false;
        }
    }));
    let mut shutdown = shutdown;

    loop {
        tokio::select! {
            // biased so a pending shutdown wins over a ready accept:
            // the proxy must stop taking new players before anything else.
            biased;

            changed = shutdown.changed() => {
                match changed {
                    Ok(()) if *shutdown.borrow() => {
                        info!("shutdown signal received; accept loop stopping");
                        break;
                    }
                    // value changed but not to true (a watcher flipping
                    // back) or sender dropped without a true: either way,
                    // keep going if a true might still come, exit when the
                    // channel is gone for good.
                    Ok(()) => continue,
                    Err(_) => {
                        info!("shutdown channel dropped; accept loop stopping");
                        break;
                    }
                }
            }

            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        warn!(%error, "accept failed; backing off briefly");
                        tokio::time::sleep(ACCEPT_FAILURE_BACKOFF).await;
                        continue;
                    }
                };

                let conn_id = state.next_conn_id();
                info!(conn = conn_id, %peer, "accepted connection");

                let slot = match try_acquire_slot(&state.limiter, &semaphore, peer.ip()) {
                    Ok(slot) => slot,
                    Err(SlotAcquireError::Limiter(kind)) => {
                        state.connection_rejections.fetch_add(1, Ordering::Relaxed);
                        warn!(conn = conn_id, %peer, reason = %kind, "per-IP connection cap reached; closing the connection");
                        continue; // stream drops -> close
                    }
                    Err(SlotAcquireError::Semaphore) => {
                        state.connection_rejections.fetch_add(1, Ordering::Relaxed);
                        match state.config().await {
                            Ok(config) => warn!(conn = conn_id, %peer, max_connections = config.advanced.max_connections, "global connection limit reached; closing the connection"),
                            Err(error) => warn!(conn = conn_id, %peer, %error, "global connection limit reached; closing the connection (limit unavailable)"),
                        }
                        continue; // stream drops -> close
                    }
                };

                let state = Arc::clone(&state);
                service.spawn_named(
                    format!("proxy.conn.{conn_id}"),
                    handle_connection(state, stream, peer, conn_id, slot),
                );
            }
        }
    }
    true
}

/// Register the accept loop on the task service.
///
/// The loop is registered as `proxy.accept-loop`; the returned
/// [`TaskId`] can be used to cancel it early. The loop observes only
/// the `watch<bool>` shutdown signal, not the task service's own
/// cancellation tokens, so for a clean drain the composition root
/// must flip the watch to `true` first and only then call
/// `TaskService::drain`. Finite connection tasks that are still parked
/// when the drain deadline passes are reported `timed_out` by the
/// service — that is the service's documented contract, not a leak.
///
/// The accept loop is the only task this function registers. Connections
/// it accepts are registered by the loop itself as `proxy.conn.<id>`.
pub fn serve(
    state: Arc<ProxyState>,
    listener: TcpListener,
    shutdown: watch::Receiver<bool>,
    service: &TaskService,
) -> TaskId {
    let service_in_task = service.clone();
    service.spawn_named("proxy.accept-loop", async move {
        accept_loop(state, listener, shutdown, service_in_task).await;
    })
}

/// The per-connection task body.
///
/// Holds the connection's [`ConnectionSlot`] (per-IP guard and global
/// permit) for the entire lifetime of the connection: the slot is
/// released exactly when the task ends, no matter how it ends.
///
/// `TCP_NODELAY` is applied here rather than only on the listener,
/// because accepted sockets do not reliably inherit it.
///
/// The body reads the client's handshake, routes on its intent and takes
/// over the connection from there. A status-intent handshake is served by
/// [`handle_status`](crate::handle_status); a login-intent handshake is
/// handed to [`crate::handle_login`], which owns the session through the
/// login exchange and the play-state byte pump. Other intents have no
/// handler yet, and the connection is closed instead of held open.
/// Any handshake failure closes the connection with a debug log — a peer
/// that cannot complete a frame is never tolerated.
pub async fn handle_connection(
    state: Arc<ProxyState>,
    mut stream: TcpStream,
    peer: SocketAddr,
    conn_id: u64,
    slot: ConnectionSlot,
) {
    let slot = slot; // held until the end of the task: releases both slots on drop

    if let Err(error) = stream.set_nodelay(true) {
        warn!(conn = conn_id, %peer, %error, "failed to set TCP_NODELAY; continuing");
    }

    let limits = match state.config().await {
        Ok(config) => HandshakeLimits::from_config(&config),
        Err(error) => {
            error!(conn = conn_id, %peer, %error, "configuration unavailable; dropping the connection before the handshake");
            return;
        }
    };
    match read_handshake(&mut stream, &limits).await {
        Ok(handshake) => {
            debug!(
                conn = conn_id,
                %peer,
                protocol = handshake.protocol,
                intent = ?handshake.intent,
                "handshake complete"
            );
            match handshake.intent {
                Intent::Status => {
                    if let Err(error) = handle_status(&state, &mut stream, &handshake).await {
                        debug!(conn = conn_id, %peer, %error, "status exchange ended with an error");
                    }
                }
                Intent::Login => {
                    if let Err(error) = crate::login::handle_login(
                        &state,
                        &mut stream,
                        peer,
                        conn_id,
                        &handshake,
                        &slot,
                    )
                    .await
                    {
                        debug!(conn = conn_id, %peer, %error, "login exchange ended with an error");
                    }
                }
                other => {
                    debug!(conn = conn_id, %peer, intent = ?other, "no handler for this intent yet; closing");
                }
            }
        }
        Err(error) => {
            debug!(conn = conn_id, %peer, %error, "handshake failed; closing the connection");
        }
    }

    debug!(
        conn = conn_id,
        %peer,
        connections_in_flight = state.limiter.current_connections(),
        "connection closed"
    );
}

#[cfg(test)]
pub(crate) mod test_helpers {
    //! Boot-time wiring emulated with the real tree: tests build a live
    //! `config` and `route` unit instead of stubbing the snapshot source, so
    //! the configuration and routing paths are exercised exactly the way the
    //! composition root wires them.
    use std::sync::Arc;

    use candyriya_config::Config;
    use candyriya_init::{Init, Pid};
    use candyriya_route::route_spec;
    use candyriya_scheduler::TaskService;

    use super::ProxyState;

    /// A state wired to a live config unit over a real tree.
    pub(crate) fn test_state(config: Config) -> ProxyState {
        test_state_with_config_unit(config).0
    }

    /// Like [`test_state`], but also hands back the tree handle and the
    /// config pid so a test can cancel the unit and observe the failure
    /// paths (`NotRunning`) the callers are built against.
    pub(crate) fn test_state_with_config_unit(config: Config) -> (ProxyState, Init, Pid) {
        let (state, init, config_pid, _route_pid) = test_state_with_route_unit(config);
        (state, init, config_pid)
    }

    /// Like [`test_state_with_config_unit`], but also hands back the route
    /// unit's pid so a test can cancel it and observe the routing failure
    /// paths (`NotRunning`) the callers are built against.
    pub(crate) fn test_state_with_route_unit(config: Config) -> (ProxyState, Init, Pid, Pid) {
        let service = TaskService::builder()
            .handle(tokio::runtime::Handle::current())
            .blocking_threads(1)
            .build()
            .expect("task service builds");
        let init = Init::new(service);
        let store = Arc::new(candyriya_config::ConfigStore::new(config));
        let config_pid = init
            .spawn_unit(Pid::INIT, crate::config_unit::config_spec(store))
            .expect("config unit spawns");
        let router_store = candyriya_route::router_store(None);
        let route_pid = init
            .spawn_unit(Pid::INIT, route_spec(Arc::clone(&router_store)))
            .expect("route unit spawns");
        (
            ProxyState::new(init.clone(), config_pid, route_pid, router_store),
            init,
            config_pid,
            route_pid,
        )
    }
}
