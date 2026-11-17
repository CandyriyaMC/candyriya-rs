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

use arc_swap::ArcSwap;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tracing::{debug, info, warn};

use candyriya_auth::AuthConfig;
use candyriya_config::Config;
use candyriya_ratelimit::ConnectionLimiter;
use candyriya_route::BackendRegistry;
use candyriya_scheduler::{TaskId, TaskService};

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
/// Deliberately minimal: the configuration, a monotonic connection
/// counter, and the connection limiter. Everything else the proxy needs
/// is registered as its own task on the service — shared state stays
/// small on purpose.
pub struct ProxyState {
    /// The loaded proxy configuration. Live: a `/candyriya reload` swap
    /// is visible to freshly accepted connections and status/login
    /// handling without a restart. In-flight connections keep the
    /// snapshot they started with.
    pub config: ArcSwap<Config>,
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
    /// Backend routing registry (try chain + forced hosts); `None` until
    /// the composition root (or tests) wires one in. Without it, login
    /// connections get a clean "no backend available" disconnect. Swapped
    /// atomically on reload; readers hold an `Arc` to one registry.
    pub router: ArcSwap<Option<Arc<BackendRegistry>>>,
    /// Connections refused before they were served (per-IP cap or the
    /// global `max-connections` semaphore). Read by the composition root
    /// for the status snapshot and metrics.
    pub connection_rejections: AtomicU64,
    /// Players currently in the play state, with the backend they landed
    /// on. Populated on the transition to play, drained when the byte pump
    /// ends; the source of truth for `glist` and the player counters.
    pub roster: PlayerRoster,
}

impl core::fmt::Debug for ProxyState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let max_connections = self.config.load().advanced.max_connections;
        f.debug_struct("ProxyState")
            .field("max_connections", &max_connections)
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
    /// State from the loaded configuration alone.
    ///
    /// The per-IP cap falls back to [`crate::DEFAULT_PER_IP_MAX`]
    /// because `[advanced]` has no per-IP key yet.
    pub fn new(config: Arc<Config>) -> Self {
        Self::with_limits(config, crate::DEFAULT_PER_IP_MAX)
    }

    /// State from the configuration with an explicit per-IP cap.
    ///
    /// Split from [`ProxyState::new`] so tests (and, later, a config
    /// key) can pin the per-IP cap without mutating the config crate.
    pub fn with_limits(config: Arc<Config>, per_ip_max: u64) -> Self {
        // The limiter's own global cap is unbounded by design: the
        // semaphore in the accept loop is the single owner of the
        // global cap (see the `limit` module docs).
        let limiter = ConnectionLimiter::with_per_ip_max(u64::MAX, per_ip_max);
        Self {
            config: ArcSwap::new(config),
            conn_counter: AtomicU64::new(0),
            limiter,
            favicon_cache: None,
            auth: AuthConfig::default(),
            router: ArcSwap::from_pointee(None),
            connection_rejections: AtomicU64::new(0),
            roster: PlayerRoster::new(),
        }
    }

    /// State with a backend router pre-wired (the composition root's job;
    /// tests wire their own fixtures).
    pub fn with_router(self, router: Arc<BackendRegistry>) -> Self {
        self.set_router(Some(router));
        self
    }

    /// The current routing registry, when one is wired in.
    ///
    /// The returned `Arc` keeps that exact registry alive even if a reload
    /// swaps in a rebuilt one concurrently.
    pub fn router(&self) -> Option<Arc<BackendRegistry>> {
        self.router.load_full().as_ref().clone()
    }

    /// Atomically replace the routing registry.
    ///
    /// Called by the reload path after the backend set is rebuilt from the
    /// new configuration. `None` disables routing entirely (no `[servers]`
    /// configured); readers that grabbed the old registry keep using it
    /// until they drop it — never a half-swapped route table.
    pub fn set_router(&self, router: Option<Arc<BackendRegistry>>) {
        self.router.store(Arc::new(router));
    }

    /// The current configuration snapshot.
    ///
    /// Cheap (an atomic load plus a refcount bump). In-flight connections
    /// keep the snapshot they started with; fresh readers observe a reload.
    pub fn config(&self) -> Arc<Config> {
        self.config.load_full()
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
pub async fn accept_loop(
    state: Arc<ProxyState>,
    listener: TcpListener,
    shutdown: watch::Receiver<bool>,
    service: TaskService,
) {
    let semaphore = Arc::new(Semaphore::new(
        usize::try_from(state.config().advanced.max_connections).unwrap_or(usize::MAX),
    ));
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
                        warn!(conn = conn_id, %peer, max_connections = state.config().advanced.max_connections, "global connection limit reached; closing the connection");
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
    service.spawn_named(
        "proxy.accept-loop",
        accept_loop(state, listener, shutdown, service_in_task),
    )
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

    let limits = HandshakeLimits::from_config(&state.config());
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
