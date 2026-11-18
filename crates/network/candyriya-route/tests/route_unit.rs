//! The `route` unit over a real tree: spawned with the same spec the
//! composition root spawns, fed by a shared store the reload path swaps
//! through, and queried exactly the way the proxy and the CLI query it.
//! No fakes, no real Minecraft backends — only loopback TCP where a health
//! check needs one.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use candyriya_protocol::{DEFAULT_MAX_STRING_LENGTH, Packet, PacketCodec, write_string};
use candyriya_route::{
    BackendRegistry, BackendState, DEFAULT_HEALTH_CHECK_INTERVAL, ForcedHosts, RouteMsg, RouteResp,
    RouterStore, health_spec, route_spec, router_store,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::codec::{Decoder, Encoder};

use candyriya_init::{CallError, ExitStatus, Init, Pid, UnitSpec, UnitState};
use candyriya_scheduler::TaskService;

fn test_tree() -> Init {
    let service = TaskService::builder()
        .handle(tokio::runtime::Handle::current())
        .blocking_threads(1)
        .build()
        .expect("task service builds");
    Init::new(service)
}

fn registry(pairs: &[(&str, SocketAddr)], try_order: &[&str]) -> BackendRegistry {
    BackendRegistry::new(
        pairs.iter().map(|&(name, addr)| (name.to_owned(), addr)),
        try_order.iter().map(|&name| name.to_owned()),
    )
    .expect("registry must build")
}

/// Spawn the route unit with `store`; returns the tree and the unit's pid.
fn spawn_route(init: &Init, store: &RouterStore) -> Pid {
    init.spawn_unit(Pid::INIT, route_spec(Arc::clone(store)))
        .expect("route unit spawns")
}

async fn call(init: &Init, pid: Pid, msg: RouteMsg) -> RouteResp {
    init.call::<RouteMsg, RouteResp>(Pid::INIT, pid, msg)
        .await
        .expect("the route unit answers")
}

fn socket_addr() -> SocketAddr {
    "127.0.0.1:9".parse().expect("literal address")
}

#[tokio::test]
async fn the_unit_spawns_with_an_empty_store_and_answers_honestly() {
    let init = test_tree();
    let store = router_store(None);
    let pid = spawn_route(&init, &store);

    let reply = call(&init, pid, RouteMsg::TryChain).await;
    assert!(matches!(reply, RouteResp::Chain(chain) if chain.is_empty()));
    let reply = call(&init, pid, RouteMsg::Names).await;
    assert!(matches!(reply, RouteResp::Names(names) if names.is_empty()));
    let reply = call(&init, pid, RouteMsg::Snapshot).await;
    assert!(matches!(reply, RouteResp::Snapshot(entries) if entries.is_empty()));
    let reply = call(
        &init,
        pid,
        RouteMsg::State {
            name: "nope".to_owned(),
        },
    )
    .await;
    assert!(matches!(reply, RouteResp::State(None)));
    let reply = call(
        &init,
        pid,
        RouteMsg::SetState {
            name: "nope".to_owned(),
            state: BackendState::Offline,
        },
    )
    .await;
    assert!(matches!(reply, RouteResp::SetState(false)));
    let reply = call(
        &init,
        pid,
        RouteMsg::Resolve {
            host: "example.com".to_owned(),
        },
    )
    .await;
    assert!(matches!(reply, RouteResp::Resolved(None)));
}

#[tokio::test]
async fn forced_hosts_resolve_with_declaration_order_and_fall_back_to_nothing() {
    let init = test_tree();
    let addr_a = socket_addr();
    let addr_b = "127.0.0.1:8".parse().expect("literal address");
    let mut reg = registry(&[("a", addr_a), ("b", addr_b)], &["b", "a"]);
    reg.set_forced_hosts(
        ForcedHosts::new(vec![
            ("*.example.com".to_owned(), vec!["a".to_owned()]),
            (
                "direct.example.com".to_owned(),
                vec!["b".to_owned(), "a".to_owned()],
            ),
        ])
        .expect("forced hosts build"),
    )
    .expect("forced hosts install");
    let store = router_store(Some(Arc::new(reg)));
    let pid = spawn_route(&init, &store);

    // exact pattern beats the single-label wildcard
    let reply = call(
        &init,
        pid,
        RouteMsg::Resolve {
            host: "direct.example.com".to_owned(),
        },
    )
    .await;
    let RouteResp::Resolved(Some(backends)) = reply else {
        panic!("expected a forced-host match");
    };
    let names: Vec<&str> = backends.iter().map(|backend| backend.name()).collect();
    assert_eq!(names, ["b", "a"]);
    assert_eq!(backends[0].addr(), addr_b);

    // the wildcard covers exactly one label
    let reply = call(
        &init,
        pid,
        RouteMsg::Resolve {
            host: "one.example.com".to_owned(),
        },
    )
    .await;
    let RouteResp::Resolved(Some(backends)) = reply else {
        panic!("expected a wildcard match");
    };
    assert_eq!(backends.len(), 1);
    assert_eq!(backends[0].name(), "a");

    // two labels in place of the wildcard: no match, the caller falls back to the chain
    let reply = call(
        &init,
        pid,
        RouteMsg::Resolve {
            host: "two.levels.example.com".to_owned(),
        },
    )
    .await;
    assert!(matches!(reply, RouteResp::Resolved(None)));

    // the try chain keeps declaration order
    let reply = call(&init, pid, RouteMsg::TryChain).await;
    let RouteResp::Chain(chain) = reply else {
        panic!("expected the try chain");
    };
    let names: Vec<&str> = chain.iter().map(|backend| backend.name()).collect();
    assert_eq!(names, ["b", "a"]);
}

#[tokio::test]
async fn state_round_trips_through_the_unit_and_registration_order_is_kept() {
    let init = test_tree();
    let addr = socket_addr();
    let reg = registry(&[("zeta", addr), ("alpha", addr)], &["alpha"]);
    let store = router_store(Some(Arc::new(reg)));
    let pid = spawn_route(&init, &store);

    let reply = call(&init, pid, RouteMsg::Names).await;
    let RouteResp::Names(names) = reply else {
        panic!("expected names");
    };
    assert_eq!(names, ["zeta", "alpha"]);

    let reply = call(
        &init,
        pid,
        RouteMsg::State {
            name: "zeta".to_owned(),
        },
    )
    .await;
    assert!(matches!(
        reply,
        RouteResp::State(Some(BackendState::Unknown))
    ));

    let reply = call(
        &init,
        pid,
        RouteMsg::SetState {
            name: "zeta".to_owned(),
            state: BackendState::Offline,
        },
    )
    .await;
    assert!(matches!(reply, RouteResp::SetState(true)));
    let reply = call(
        &init,
        pid,
        RouteMsg::State {
            name: "zeta".to_owned(),
        },
    )
    .await;
    assert!(matches!(
        reply,
        RouteResp::State(Some(BackendState::Offline))
    ));

    let reply = call(
        &init,
        pid,
        RouteMsg::SetState {
            name: "ghost".to_owned(),
            state: BackendState::Online,
        },
    )
    .await;
    assert!(matches!(reply, RouteResp::SetState(false)));

    let reply = call(&init, pid, RouteMsg::Snapshot).await;
    let RouteResp::Snapshot(entries) = reply else {
        panic!("expected a snapshot");
    };
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name(), "zeta");
    assert_eq!(entries[0].state(), BackendState::Offline);
    assert_eq!(entries[1].name(), "alpha");
    assert_eq!(entries[1].state(), BackendState::Unknown);
}

#[tokio::test]
async fn a_swap_through_the_shared_store_is_seen_by_the_next_call() {
    let init = test_tree();
    let addr = socket_addr();
    let v1 = registry(&[("old", addr)], &["old"]);
    let store = router_store(Some(Arc::new(v1)));
    let pid = spawn_route(&init, &store);

    let reply = call(&init, pid, RouteMsg::Names).await;
    assert!(matches!(reply, RouteResp::Names(names) if names == ["old"]));

    // the reload path's move: build a fresh registry and swap it in
    let v2 = registry(&[("new", addr)], &["new"]);
    store.store(Arc::new(Some(Arc::new(v2))));

    let reply = call(&init, pid, RouteMsg::Names).await;
    assert!(matches!(reply, RouteResp::Names(names) if names == ["new"]));
    let reply = call(
        &init,
        pid,
        RouteMsg::State {
            name: "old".to_owned(),
        },
    )
    .await;
    assert!(matches!(reply, RouteResp::State(None)));

    // and routing can be turned off the same way
    store.store(Arc::new(None));
    let reply = call(&init, pid, RouteMsg::Names).await;
    assert!(matches!(reply, RouteResp::Names(names) if names.is_empty()));
}

#[tokio::test]
async fn a_cancelled_unit_refuses_the_call_with_not_running() {
    let init = test_tree();
    let store = router_store(None);
    let pid = spawn_route(&init, &store);
    init.cancel(pid).expect("the unit cancels");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if !matches!(
            init.unit(pid).map(|unit| unit.state),
            Some(UnitState::Running)
        ) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the route unit did not stop within the deadline"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let err = init
        .call::<RouteMsg, RouteResp>(Pid::INIT, pid, RouteMsg::Names)
        .await
        .expect_err("a down unit must not serve a reply");
    assert_eq!(err, CallError::NotRunning);
}

/// A loopback server speaking just enough of the status protocol for the
/// health check: read the handshake and the status request, answer with a
/// status response carrying `status_json`.
async fn spawn_status_server(status_json: &str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let json = status_json.to_owned();
    tokio::spawn(async move {
        while let Ok(accepted) = listener.accept().await {
            let (mut stream, _) = accepted;
            let json = json.clone();
            tokio::spawn(async move {
                let mut codec = PacketCodec::new();
                let mut pending = BytesMut::with_capacity(256);
                let mut packets_seen = 0u8;

                let mut payload = Vec::new();
                write_string(&mut payload, &json, DEFAULT_MAX_STRING_LENGTH).expect("fits");
                let mut out = BytesMut::new();
                codec
                    .encode(
                        Packet {
                            id: 0x00,
                            payload: Bytes::copy_from_slice(&payload),
                        },
                        &mut out,
                    )
                    .expect("response encodes");

                loop {
                    match stream.read_buf(&mut pending).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    while let Ok(Some(packet)) = codec.decode(&mut pending) {
                        packets_seen += 1;
                        let _ = packet;
                        if packets_seen >= 2 {
                            let _ = stream.write_all(&out).await;
                            let _ = stream.shutdown().await;
                            return;
                        }
                    }
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn the_health_child_marks_a_dead_backend_offline_and_a_live_one_online() {
    let init = test_tree();
    // a bound-then-released port: nothing is listening on it
    let probe = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let dead = probe.local_addr().expect("local addr");
    drop(probe);
    let live = spawn_status_server(r#"{"description":{"text":"up"}}"#).await;

    let reg = registry(&[("dead", dead), ("live", live)], &["live", "dead"]);
    let store = router_store(Some(Arc::new(reg)));
    let route_pid = spawn_route(&init, &store);

    let health = init
        .spawn_unit(
            route_pid,
            health_spec(Arc::clone(&store), Duration::from_millis(50)),
        )
        .expect("health child spawns under the route unit");

    /// One `State` round trip, reduced to the state alone.
    async fn state_of(init: &Init, route_pid: Pid, name: &str) -> Option<BackendState> {
        match call(
            init,
            route_pid,
            RouteMsg::State {
                name: name.to_owned(),
            },
        )
        .await
        {
            RouteResp::State(state) => state,
            RouteResp::Resolved(_)
            | RouteResp::Chain(_)
            | RouteResp::SetState(_)
            | RouteResp::Names(_)
            | RouteResp::Snapshot(_) => None,
        }
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let live_state = state_of(&init, route_pid, "live").await;
        let dead_state = state_of(&init, route_pid, "dead").await;
        if live_state == Some(BackendState::Online) && dead_state == Some(BackendState::Offline) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the health child did not settle the backends within the deadline (live: {live_state:?}, dead: {dead_state:?})"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    init.cancel(health).expect("the health child cancels");
}

#[tokio::test]
async fn a_sibling_with_a_grant_can_read_routing_and_one_without_is_refused() {
    async fn probe(init: &Init, route_pid: Pid, with_grant: bool) -> ExitStatus {
        let init_for_body = init.clone();
        let mut spec = UnitSpec::new(
            "route.probe",
            candyriya_init::UnitKind::LongLived,
            candyriya_init::RestartPolicy::Never,
            move || {
                let init = init_for_body.clone();
                async move {
                    let caller = candyriya_init::current_unit().expect("the body runs as its unit");
                    match init
                        .call::<RouteMsg, RouteResp>(caller, route_pid, RouteMsg::Names)
                        .await
                    {
                        Ok(_) => ExitStatus::Success,
                        Err(error) => ExitStatus::failure(format!("call failed: {error}")),
                    }
                }
            },
        );
        if with_grant {
            spec = spec.with_grant(route_pid);
        }
        let pid = init.spawn_unit(Pid::INIT, spec).expect("probe spawns");
        // harvest the probe's outcome through the kernel's records
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            for outcome in init.reap_children(Pid::INIT) {
                let record = match outcome {
                    candyriya_init::ReapOutcome::Reaped(record)
                    | candyriya_init::ReapOutcome::Restarting { record, .. }
                    | candyriya_init::ReapOutcome::FailedTerminal { record } => record,
                };
                if record.pid == pid {
                    return record.status;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the probe did not finish within the deadline"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    let init = test_tree();
    let store = router_store(None);
    let route_pid = spawn_route(&init, &store);
    let ok = probe(&init, route_pid, true).await;
    assert!(matches!(ok, ExitStatus::Success), "grant holds: {ok}");
    let denied = probe(&init, route_pid, false).await;
    assert!(
        !matches!(denied, ExitStatus::Success),
        "no grant, no read: {denied}"
    );
}

#[test]
fn the_health_interval_constant_is_sane_and_documented() {
    assert_eq!(DEFAULT_HEALTH_CHECK_INTERVAL, Duration::from_secs(30));
}
