//! Behavior tests for the per-connection counters and the read models ps
//! consumes (`connections` / `connection_info`).
//!
//! Connection units are ordinary units with one extra: byte counters bound
//! at spawn, bumped through the task-local the body sees, read back through
//! the tree without holding the registry lock on the hot path.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use candyriya_init::{
    ExitStatus, Init, Pid, RestartPolicy, UnitKind, UnitSpec, UnitState, current_conn_stats,
};
use candyriya_scheduler::TaskService;
use tokio::time::Instant;

fn tree() -> Init {
    let service = TaskService::builder()
        .handle(tokio::runtime::Handle::current())
        .blocking_threads(2)
        .build()
        .expect("service builds");
    Init::new(service)
}

async fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for condition");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_unit_reports_counters_until_reaped() {
    let tree = tree();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "conn-1",
                UnitKind::Connection,
                RestartPolicy::Never,
                || async {
                    let stats = current_conn_stats().expect("a connection unit carries counters");
                    stats.add_bytes_rx(10);
                    stats.add_bytes_rx(5);
                    stats.add_bytes_tx(4);
                    ExitStatus::Success
                },
            ),
        )
        .expect("unit spawns");

    wait_until(
        || {
            tree.unit(pid)
                .is_some_and(|unit| unit.state == UnitState::Zombie)
        },
        Duration::from_secs(2),
    )
    .await;

    let info = tree
        .connection_info(pid)
        .expect("a zombie connection unit keeps its counters");
    assert_eq!(info.pid, pid);
    assert_eq!(info.name, "conn-1");
    assert_eq!(info.bytes_rx, 15);
    assert_eq!(info.bytes_tx, 4);
    assert_eq!(info.state, UnitState::Zombie);
    assert!(!info.parked);
    assert!(info.connected_at <= info.last_activity);

    let _ = tree.reap_children(Pid::INIT);
    assert!(
        tree.connection_info(pid).is_none(),
        "a reaped connection unit leaves no read model behind"
    );
    assert!(
        tree.connections().iter().all(|info| info.pid != pid),
        "a reaped connection unit leaves the connection list"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_connection_units_carry_no_counters() {
    let tree = tree();
    let saw_stats = Arc::new(AtomicBool::new(false));
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new("worker", UnitKind::LongLived, RestartPolicy::Never, {
                let saw_stats = Arc::clone(&saw_stats);
                move || {
                    let saw_stats = Arc::clone(&saw_stats);
                    async move {
                        saw_stats.store(current_conn_stats().is_some(), Ordering::SeqCst);
                        ExitStatus::Success
                    }
                }
            }),
        )
        .expect("unit spawns");
    wait_until(
        || {
            tree.unit(pid)
                .is_some_and(|unit| unit.state == UnitState::Zombie)
        },
        Duration::from_secs(2),
    )
    .await;
    assert!(!saw_stats.load(Ordering::SeqCst));
    assert!(tree.connection_info(pid).is_none());
    assert!(tree.connections().iter().all(|info| info.pid != pid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connections_are_listed_in_spawn_order() {
    let tree = tree();
    let one = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "conn-100",
                UnitKind::Connection,
                RestartPolicy::Never,
                || async { ExitStatus::Success },
            ),
        )
        .expect("unit spawns");
    let not_a_conn = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new("noise", UnitKind::Oneshot, RestartPolicy::Never, || async {
                ExitStatus::Success
            }),
        )
        .expect("unit spawns");
    let two = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "conn-101",
                UnitKind::Connection,
                RestartPolicy::Never,
                || async { ExitStatus::Success },
            ),
        )
        .expect("unit spawns");
    assert!(one < not_a_conn && not_a_conn < two);

    let pids: Vec<Pid> = tree
        .connections()
        .into_iter()
        .map(|info| info.pid)
        .collect();
    assert_eq!(pids, vec![one, two]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_activity_advances_with_traffic() {
    let tree = tree();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "conn-102",
                UnitKind::Connection,
                RestartPolicy::Never,
                || async {
                    let stats = current_conn_stats().expect("a connection unit carries counters");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    stats.add_bytes_rx(1);
                    ExitStatus::Success
                },
            ),
        )
        .expect("unit spawns");

    wait_until(
        || {
            tree.connection_info(pid)
                .is_some_and(|info| info.bytes_rx == 1)
        },
        Duration::from_secs(2),
    )
    .await;
    let info = tree.connection_info(pid).expect("connection info exists");
    assert!(
        info.last_activity >= info.connected_at + Duration::from_millis(10),
        "last activity must reflect the byte that flowed (connected_at + >=10ms), got {:?}",
        info.last_activity.duration_since(info.connected_at),
    );
    let _ = tree.reap_children(Pid::INIT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parked_flag_is_visible_on_connection_units() {
    let tree = tree();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "conn-103",
                UnitKind::Connection,
                RestartPolicy::Never,
                std::future::pending::<ExitStatus>,
            ),
        )
        .expect("unit spawns");
    assert!(!tree.connection_info(pid).expect("info").parked);
    let _ = tree.freeze(pid);
    assert!(tree.connection_info(pid).expect("info").parked);
    let _ = tree.unfreeze(pid);
    assert!(!tree.connection_info(pid).expect("info").parked);
    assert!(tree.cancel(pid).expect("cancel"));
    let _ = tree.reap_children(Pid::INIT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_info_is_none_for_reaped_pids() {
    let tree = tree();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "conn-104",
                UnitKind::Connection,
                RestartPolicy::Never,
                || async { ExitStatus::Success },
            ),
        )
        .expect("unit spawns");
    wait_until(
        || {
            tree.unit(pid)
                .is_some_and(|unit| unit.state == UnitState::Zombie)
        },
        Duration::from_secs(2),
    )
    .await;
    let _ = tree.reap_children(Pid::INIT);
    assert!(tree.connection_info(pid).is_none());
}
