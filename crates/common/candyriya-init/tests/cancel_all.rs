//! Behavior tests for the tree-wide cancellation sweep.
//!
//! `cancel_all` is the shutdown cascade's one lever: every non-root unit,
//! newest first, one call. These tests pin the ordering contract and the
//! root's exemption; the wall-clock behavior (units die at their next
//! await) is covered by the per-unit cancel tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use candyriya_init::{
    ExitStatus, Init, Pid, ReapOutcome, RestartPolicy, UnitKind, UnitSpec, UnitState,
};
use candyriya_scheduler::TaskService;
use tokio::time::Instant;

fn service() -> TaskService {
    TaskService::builder()
        .handle(tokio::runtime::Handle::current())
        .blocking_threads(2)
        .build()
        .expect("service builds")
}

fn tree() -> Init {
    Init::new(service())
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
async fn cancel_all_sweeps_newest_first_and_spares_the_root() {
    let tree = tree();
    let counter = Arc::new(AtomicUsize::new(0));
    let mut pids = Vec::new();
    for name in ["one", "two", "three"] {
        let counter = Arc::clone(&counter);
        let pid = tree
            .spawn_unit(
                Pid::INIT,
                UnitSpec::new(name, UnitKind::LongLived, RestartPolicy::Never, move || {
                    let counter = Arc::clone(&counter);
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        std::future::pending::<ExitStatus>().await
                    }
                }),
            )
            .expect("unit spawns");
        pids.push(pid);
    }
    // every body registered
    wait_until(
        || counter.load(Ordering::SeqCst) == 3,
        Duration::from_secs(2),
    )
    .await;

    let swept = tree.cancel_all();
    assert_eq!(
        swept,
        pids.iter().rev().copied().collect::<Vec<_>>(),
        "the sweep runs newest-first"
    );
    assert!(!swept.contains(&Pid::INIT));

    wait_until(
        || {
            pids.iter().all(|pid| {
                tree.unit(*pid)
                    .is_some_and(|unit| unit.state == UnitState::Zombie)
            })
        },
        Duration::from_secs(2),
    )
    .await;
    let outcomes = tree.reap_children(Pid::INIT);
    for pid in &pids {
        assert!(outcomes.iter().any(|outcome| matches!(
            outcome,
            ReapOutcome::Reaped(record)
                if record.pid == *pid && record.status == ExitStatus::Cancelled
        )));
    }
    assert_eq!(tree.units().len(), 1, "only the root remains");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_all_on_a_root_only_tree_is_empty() {
    let tree = tree();
    assert!(tree.cancel_all().is_empty());
    assert_eq!(tree.units().len(), 1);
    let root = tree.unit(Pid::INIT).expect("root exists");
    assert_eq!(root.state, UnitState::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_all_skips_units_that_already_exited() {
    let tree = tree();
    let oneshot = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new("done", UnitKind::Oneshot, RestartPolicy::Never, || async {
                ExitStatus::Success
            }),
        )
        .expect("unit spawns");
    wait_until(
        || {
            tree.unit(oneshot)
                .is_some_and(|unit| unit.state == UnitState::Zombie)
        },
        Duration::from_secs(2),
    )
    .await;
    let live = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new("live", UnitKind::LongLived, RestartPolicy::Never, || {
                std::future::pending::<ExitStatus>()
            }),
        )
        .expect("unit spawns");

    let swept = tree.cancel_all();
    assert_eq!(
        swept,
        vec![live],
        "the zombie is not listed; the live unit is"
    );
    let _ = tree.reap_children(Pid::INIT);
}
