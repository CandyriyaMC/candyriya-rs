//! Behavior tests for the cooperative park gate (`freeze` / `unfreeze`).
//!
//! No network, no sleeps longer than strictly necessary; every wait on the
//! implementation is a bounded poll, so the suite is deterministic on loaded
//! machines. The gate starts open, so a frozen unit is one the tree parked
//! explicitly.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use candyriya_init::{
    ExitStatus, Init, InitError, Pid, ReapOutcome, RestartPolicy, UnitKind, UnitSpec, UnitState,
    park_gate,
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

/// Spawn a body that holds at the gate until the test lets it through,
/// then waits on the gate itself and reports through `bumped`.
///
/// The arming channel is what makes the test deterministic: the body cannot
/// observe the gate before the test has had a chance to close it. The arm
/// rides a slot because unit bodies are factories (`Fn`, not `FnOnce`).
fn gated_body(
    arm: tokio::sync::oneshot::Receiver<()>,
    bumped: Arc<AtomicUsize>,
) -> impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ExitStatus> + Send>> {
    let arm = Arc::new(parking_lot::Mutex::new(Some(arm)));
    move || {
        let taken = arm.lock().take();
        let bumped = Arc::clone(&bumped);
        Box::pin(async move {
            let Some(arm) = taken else {
                return ExitStatus::failure("body rebuilt without its arm");
            };
            let _ = arm.await;
            match park_gate() {
                Some(mut gate) => {
                    gate.wait_open().await;
                    bumped.fetch_add(1, Ordering::SeqCst);
                    ExitStatus::Success
                }
                None => {
                    bumped.fetch_add(100, Ordering::SeqCst);
                    ExitStatus::failure("no park gate bound")
                }
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frozen_unit_parks_until_unfrozen() {
    let tree = tree();
    let bumped = Arc::new(AtomicUsize::new(0));
    let (arm_tx, arm_rx) = tokio::sync::oneshot::channel();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "gated",
                UnitKind::LongLived,
                RestartPolicy::Never,
                gated_body(arm_rx, Arc::clone(&bumped)),
            ),
        )
        .expect("unit spawns");
    assert!(!tree.is_parked(pid).expect("is_parked"));

    // close the gate before the body is allowed to look at it
    assert!(tree.freeze(pid).expect("freeze"));
    // a second freeze is a no-op, not an error
    assert!(!tree.freeze(pid).expect("freeze"));
    assert!(tree.is_parked(pid).expect("is_parked"));
    assert!(tree.unit(pid).expect("unit exists").parked);

    arm_tx.send(()).expect("body is armed");
    // the body must now be parked at the gate; give it ample room to prove it
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        bumped.load(Ordering::SeqCst),
        0,
        "a frozen body must not pass its gate"
    );

    assert!(tree.unfreeze(pid).expect("unfreeze"));
    // a second unfreeze is a no-op
    assert!(!tree.unfreeze(pid).expect("unfreeze"));
    assert!(!tree.is_parked(pid).expect("is_parked"));
    assert!(!tree.unit(pid).expect("unit exists").parked);

    wait_until(
        || bumped.load(Ordering::SeqCst) == 1,
        Duration::from_secs(2),
    )
    .await;
    wait_until(
        || {
            tree.unit(pid)
                .is_some_and(|unit| unit.state == UnitState::Zombie)
        },
        Duration::from_secs(2),
    )
    .await;
    let outcomes = tree.reap_children(Pid::INIT);
    assert!(outcomes.iter().any(|outcome| matches!(
        outcome,
        ReapOutcome::Reaped(record) if record.pid == pid && record.status == ExitStatus::Success
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_drops_a_parked_body_as_cancelled() {
    let tree = tree();
    let bumped = Arc::new(AtomicUsize::new(0));
    let (arm_tx, arm_rx) = tokio::sync::oneshot::channel();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "gated",
                UnitKind::LongLived,
                RestartPolicy::Never,
                gated_body(arm_rx, Arc::clone(&bumped)),
            ),
        )
        .expect("unit spawns");
    assert!(tree.freeze(pid).expect("freeze"));
    arm_tx.send(()).expect("body is armed");
    // let the body reach the parked await before cancelling
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(tree.cancel(pid).expect("cancel"));

    wait_until(
        || {
            tree.unit(pid)
                .is_some_and(|unit| unit.state == UnitState::Zombie)
        },
        Duration::from_secs(2),
    )
    .await;
    let outcomes = tree.reap_children(Pid::INIT);
    assert!(outcomes.iter().any(|outcome| matches!(
        outcome,
        ReapOutcome::Reaped(record) if record.pid == pid && record.status == ExitStatus::Cancelled
    )));
    assert_eq!(
        bumped.load(Ordering::SeqCst),
        0,
        "a parked-then-cancelled body never passes its gate"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_flips_on_a_unit_that_already_exited() {
    let tree = tree();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "instant",
                UnitKind::Oneshot,
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
    // accepted and recorded, like cancel() on a zombie: no body observes it
    assert!(tree.freeze(pid).expect("freeze"));
    assert!(tree.is_parked(pid).expect("is_parked"));
    assert!(tree.unfreeze(pid).expect("unfreeze"));
    assert!(!tree.is_parked(pid).expect("is_parked"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeze_and_unfreeze_reject_reaped_pids() {
    let tree = tree();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "instant",
                UnitKind::Oneshot,
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
    assert!(
        tree.unit(pid).is_none(),
        "a reaped unit leaves the registry"
    );
    assert!(matches!(tree.freeze(pid), Err(InitError::NoSuchUnit(_))));
    assert!(matches!(tree.unfreeze(pid), Err(InitError::NoSuchUnit(_))));
    assert!(matches!(tree.is_parked(pid), Err(InitError::NoSuchUnit(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn park_gate_is_absent_outside_unit_bodies() {
    assert!(
        park_gate().is_none(),
        "the gate is bound only for unit runs"
    );
}
