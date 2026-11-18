//! Per-connection supervised units: `conn-<n>`, children of `players`.
//!
//! Every accepted connection becomes a unit through the tree — an
//! [`UnitKind::Connection`] node registered before any byte is read,
//! carrying exact byte counters (see
//! [`current_conn_stats`](candyriya_init::current_conn_stats)) and a
//! cooperative park gate (see [`park_gate`](candyriya_init::park_gate))
//! from birth to reap. The spawn goes through the same
//! [`Init::spawn_unit`](candyriya_init::Init::spawn_unit) path as every
//! other unit; the accept loop is not special.
//!
//! Naming and policy: the name is `conn-<n>` where `n` is the connection
//! counter handed out by the accept loop (monotonic, starting at 1), and
//! the restart policy is [`RestartPolicy::Never`] — a connection is not
//! restartable; a reconnected player is a new unit with a new pid. The
//! kind already existed in the closed [`UnitKind`] vocabulary (`Connection`
//! is rendered by ps as its own category), so no new variant was added.
//!
//! Capabilities ride the spawn like inherited fds: the body itself makes
//! calls (config fetches, routing, the favicon cache) attributed to the
//! connection's own pid, which is its own unit — the subtree rule does not
//! authorize sibling calls, so the handles are installed here and passed
//! down. Legality is checked by the kernel against the *parent* (`players`),
//! which holds the same three grants; anything else is a spawn-time error,
//! surfaced as a refused connection rather than a half-registered unit.
//!
//! Shutdown scoping comes free: `conn-*` units are children of `players`,
//! so the tree-wide cancel sweep reaches them like everything else; the
//! pump parks at its next loop boundary and the wrapper records
//! [`ExitStatus::Cancelled`] when the body is dropped.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::net::TcpStream;

use candyriya_init::{ExitStatus, InitError, Pid, RestartPolicy, UnitKind, UnitSpec};

use crate::accept::ProxyState;
use crate::limit::ConnectionSlot;

/// Build the spec for one connection unit and spawn it as a child of
/// `players`.
///
/// The body factory runs once (the policy is `Never`); the socket, the
/// peer address and the limit slot move into the body. A spawn that fails
/// (the `players` group is gone or a grant is unavailable) surfaces as an
/// error — the caller drops the connection instead of leaving it
/// unregistered.
///
/// # Errors
///
/// [`InitError`] — whatever the tree reports at spawn time.
pub(crate) fn spawn_conn_unit(
    state: &Arc<ProxyState>,
    stream: TcpStream,
    peer: SocketAddr,
    conn_id: u64,
    slot: ConnectionSlot,
) -> Result<Pid, InitError> {
    state.init().spawn_unit(
        state.players_pid(),
        conn_spec(state, stream, peer, conn_id, slot),
    )
}

fn conn_spec(
    state: &Arc<ProxyState>,
    stream: TcpStream,
    peer: SocketAddr,
    conn_id: u64,
    slot: ConnectionSlot,
) -> UnitSpec {
    // grants are resolved before the clone moves into the factory
    let config_pid = state.config_pid();
    let route_pid = state.route_pid();
    // the favicon handle exists only when the composition root wired one;
    // granting to a unit that does not exist would fail the spawn
    let cache_pid = state.favicon_cache.as_ref().map(|cache| cache.cache_pid());
    // the factory is `Fn`, not `FnOnce`, so the socket rides a slot the one
    // invocation a Never-policy unit ever gets takes; a re-invocation reports
    // a failure instead of serving on somebody else's socket
    let args = Arc::new(Mutex::new(Some((stream, peer, slot))));
    let state = Arc::clone(state);
    let mut spec = UnitSpec::new(
        format!("conn-{conn_id}"),
        UnitKind::Connection,
        RestartPolicy::Never,
        move || {
            let taken = args.lock().ok().and_then(|mut slot| slot.take());
            let state = Arc::clone(&state);
            async move {
                match taken {
                    Some((stream, peer, slot)) => {
                        crate::accept::handle_connection(state, stream, peer, conn_id, slot).await
                    }
                    None => ExitStatus::failure("connection body rebuilt without its socket"),
                }
            }
        },
    )
    .with_grant(config_pid)
    .with_grant(route_pid);
    if let Some(cache_pid) = cache_pid {
        spec = spec.with_grant(cache_pid);
    }
    spec
}
