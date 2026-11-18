//! The `host` unit: the supervised owner of the plugin-facing core services.
//!
//! Plugin host functions (`candyriya:*`) used to reach the dispatcher, the
//! play-state roster and the backend name list through a global service
//! registry. This unit is their home instead: one long-lived, `Plugins`
//! kind unit, spawned by the composition root right after the shared
//! proxy state exists. Everything it answers is either read straight off
//! the state it holds (the roster — live, no lag), fanned into the
//! shared dispatcher (`callApi`), or fetched over the tree (`Names`, from
//! the `route` unit).
//!
//! # Why the protocol lives here
//!
//! The message pair is the contract between the composition root (which
//! spawns the unit and attaches the handler) and its callers (the plugin
//! crate's host functions). It lives in [`candyriya_plugin`] next to the
//! rest of the host-function plumbing; this file is the serving side.
//! Three request variants, three reply variants, zero serialization —
//! the tree type-erases envelopes and downcasts on the callee. Internal
//! workspace wiring, not a stabilized API; adding a variant is an
//! explicit, reviewable change.
//!
//! # Capability
//!
//! Callers on plugin threads are not tree units; they attribute to
//! [`Pid::INIT`], which is an ancestor of this unit, so the subtree rule
//! is enough and no grant is needed. The unit's own outbound call (the
//! `Names` fetch into the `route` unit) runs inside this unit's body
//! with the unit itself as the caller; the route unit is a sibling, so
//! the spawner installs the grant on this unit's spawn, like an
//! inherited fd.
//!
//! # Reply cost
//!
//! The roster reply is a fresh `Vec<PlayerRef>` materialized from the
//! live play-state roster — the same shape and cost as the snapshot the
//! old registry registration carried, but with no 1s refresh window in
//! between join and visibility. `callApi` is a synchronous mailbox
//! round trip into the target plugin with a 10s bound, so it runs on
//! the service's blocking pool rather than on the caller's tokio
//! worker. Note what that implies for nested `callApi` chains (A calls
//! B, B calls back through this unit): the unit answers envelopes in
//! order, so B's nested call waits for A's envelope to finish and
//! fails with the tree's 5s `Timeout` instead of recursing as it did
//! when every plugin thread reached the dispatcher directly. Bounded,
//! reported honestly, and the common (non-recursive) fan-out is
//! unaffected.
//!
//! # Failure vocabulary
//!
//! Callers see the tree's [`candyriya_init::CallError`]. A missing or
//! crashed unit fails fast with `NotRunning`; a live-but-slow one with
//! `Timeout`. The unit runs under [`RestartPolicy::Always`] — losing it
//! would silence every plugin's cross-subsystem calls — but every call
//! site decides for itself how to fail, and the honest answers are
//! documented on the host functions. `Names` degrades to an empty list
//! (the `listServers` stand-in), never to a fabricated backend name.

use std::sync::Arc;

use tokio::sync::oneshot;
use tracing::{debug, error};

use candyriya_init::{Init, Pid, RestartPolicy, ServiceKind, UnitKind, UnitSpec, current_unit};
use candyriya_plugin::{HostMsg, HostResp, ProxyEventDispatcher};
use candyriya_plugin_api::PlayerRef;
use candyriya_route::{RouteMsg, RouteResp, route_call};
use candyriya_scheduler::TaskService;

use crate::ProxyState;

/// Describe the `host` unit for spawning.
///
/// The composition root hands in the shared proxy state (for the
/// dispatcher wiring and the live roster), the shared dispatcher (for
/// the `callApi` fan-out — the same instance the proxy fires its own
/// lifecycle events into), the `route` unit's pid, the tree and its
/// task service. The unit holds all of them for its lifetime.
///
/// The grant to `route_pid` rides the spawn: the unit's `Names` handler
/// resolves it per call, from inside the unit body, where the caller is
/// the unit itself. A missing grant degrades `Names` to an empty list,
/// never to a wrong answer.
///
/// The unit runs `LongLived` under an `Always` policy, as a direct
/// child of the root.
#[must_use]
pub fn host_spec(
    state: Arc<ProxyState>,
    dispatcher: Arc<ProxyEventDispatcher>,
    route_pid: Pid,
    init: Init,
    service: TaskService,
) -> UnitSpec {
    UnitSpec::service(
        "host",
        UnitKind::LongLived,
        RestartPolicy::Always,
        move |_ctx, msg: HostMsg| {
            let state = Arc::clone(&state);
            let dispatcher = Arc::clone(&dispatcher);
            let init = init.clone();
            let service = service.clone();
            async move { handle(state, dispatcher, route_pid, init, service, msg).await }
        },
    )
    .with_service_kind(ServiceKind::Plugins)
    .with_grant(route_pid)
}

/// Serve one message against the shared state.
async fn handle(
    state: Arc<ProxyState>,
    dispatcher: Arc<ProxyEventDispatcher>,
    route_pid: Pid,
    init: Init,
    service: TaskService,
    msg: HostMsg,
) -> HostResp {
    match msg {
        HostMsg::CallApi {
            target_plugin,
            function,
            args,
            caller,
        } => handle_call_api(dispatcher, service, target_plugin, function, args, caller).await,
        HostMsg::Roster => HostResp::Roster(roster_players(&state)),
        HostMsg::Names => {
            let caller = current_unit().unwrap_or(Pid::INIT);
            match route_call(&init, caller, route_pid, RouteMsg::Names).await {
                Ok(RouteResp::Names(names)) => HostResp::Names(names),
                Ok(other) => {
                    error!(reply = ?other, "unexpected route unit reply to a Names call; names served as empty");
                    HostResp::Names(Vec::new())
                }
                Err(error) => {
                    error!(%error, "route unit unavailable; names served as empty");
                    HostResp::Names(Vec::new())
                }
            }
        }
    }
}

/// Fan a `callApi` into the target plugin on the blocking pool.
///
/// The dispatcher is synchronous — a bounded mailbox round trip into the
/// target plugin's own thread — so awaiting it inline would park a core
/// worker for the duration. The oneshot carries the outcome back; a
/// panicked dispatch is contained by the blocking pool and surfaces as a
/// dropped channel, answered here as an honest failure, never as a
/// fabricated target response.
async fn handle_call_api(
    dispatcher: Arc<ProxyEventDispatcher>,
    service: TaskService,
    target_plugin: String,
    function: String,
    args: Vec<u8>,
    caller: Option<String>,
) -> HostResp {
    let (tx, rx) = oneshot::channel();
    let task_target = target_plugin.clone();
    let task_function = function.clone();
    service.spawn_blocking("host.call-api", move || {
        let outcome =
            match dispatcher.call_api(&task_target, &task_function, &args, caller.as_deref()) {
                Ok(output) => Ok(output),
                Err(error) => Err(error.to_string()),
            };
        let _ = tx.send(outcome);
    });
    match rx.await {
        Ok(Ok(output)) => {
            debug!(%target_plugin, %function, "callApi answered");
            HostResp::CallApi(Ok(output))
        }
        Ok(Err(message)) => {
            debug!(%target_plugin, %function, %message, "callApi declined");
            HostResp::CallApi(Err(message))
        }
        Err(_) => {
            error!(%target_plugin, %function, "dispatcher went away during the callApi");
            HostResp::CallApi(Err("host dispatcher went away during the call".to_owned()))
        }
    }
}

/// Snapshot the live play-state roster as [`PlayerRef`]s.
///
/// Cheap and consistent with the `glist` view: one locked pass over the
/// roster, sorted the same way, cloned out as owned pairs. Reading it
/// straight off the state (instead of a periodically swapped snapshot)
/// is what makes joins and parts visible on the very next call.
fn roster_players(state: &ProxyState) -> Vec<PlayerRef> {
    state
        .roster
        .snapshot()
        .into_iter()
        .map(|entry| PlayerRef {
            uuid: entry.uuid.to_string(),
            name: entry.name,
        })
        .collect()
}
