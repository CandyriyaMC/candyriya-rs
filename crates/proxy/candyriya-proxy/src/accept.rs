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

use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tracing::{debug, info, warn};

use candyriya_config::Config;
use candyriya_ratelimit::ConnectionLimiter;
use candyriya_scheduler::{TaskId, TaskService};

use crate::limit::{ConnectionSlot, SlotAcquireError, try_acquire_slot};

/// Inbound drain buffer size for the connection task (64 KiB).
const DRAIN_BUFFER_SIZE: usize = 64 * 1024;

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
    /// The loaded proxy configuration.
    pub config: Arc<Config>,
    /// Monotonic connection-id counter. Starts at 0; handed out ids
    /// start at 1.
    pub conn_counter: AtomicU64,
    /// Concurrent-connection accounting: per-IP cap plus (unbounded)
    /// limiter-side global accounting. The effective global cap is the
    /// semaphore the accept loop creates from `[advanced]
    /// max-connections`.
    pub limiter: ConnectionLimiter,
}

impl core::fmt::Debug for ProxyState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ProxyState")
            .field("max_connections", &self.config.advanced.max_connections)
            .field("conn_counter", &self.conn_counter.load(Ordering::Relaxed))
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
            config,
            conn_counter: AtomicU64::new(0),
            limiter,
        }
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
        usize::try_from(state.config.advanced.max_connections).unwrap_or(usize::MAX),
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
                        warn!(conn = conn_id, %peer, reason = %kind, "per-IP connection cap reached; closing the connection");
                        continue; // stream drops -> close
                    }
                    Err(SlotAcquireError::Semaphore) => {
                        warn!(conn = conn_id, %peer, max_connections = state.config.advanced.max_connections, "global connection limit reached; closing the connection");
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
/// The body then drains inbound bytes until EOF (or a read error) and
/// closes the connection. That is the whole job of this slice — the
/// handshake pipeline takes over in a later one. An idle client that
/// never sends anything holds the connection open, which is the correct
/// behaviour for a proxy whose next step is the handshake timeout.
pub async fn handle_connection(
    state: Arc<ProxyState>,
    mut stream: TcpStream,
    peer: SocketAddr,
    conn_id: u64,
    slot: ConnectionSlot,
) {
    let _slot = slot; // dropped at the end of the task: releases both slots

    if let Err(error) = stream.set_nodelay(true) {
        warn!(conn = conn_id, %peer, %error, "failed to set TCP_NODELAY; continuing");
    }

    let mut buffer = vec![0_u8; DRAIN_BUFFER_SIZE];
    let mut drained: u64 = 0;
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) => break, // clean EOF: the peer closed the connection
            Ok(n) => drained += n as u64,
            Err(error) => {
                debug!(conn = conn_id, %peer, %error, "read failed; closing the connection");
                break;
            }
        }
    }

    debug!(
        conn = conn_id,
        %peer,
        bytes_drained = drained,
        connections_in_flight = state.limiter.current_connections(),
        "connection closed after drain"
    );
}
