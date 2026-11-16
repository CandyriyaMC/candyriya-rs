//! Behavior tests for the task service.
//!
//! No network, no sleeps longer than strictly necessary; every wait on the
//! implementation is a bounded poll, so the suite is deterministic on
//! loaded machines.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use candyriya_scheduler::{ScheduleError, ScopeError, ScopeLimits, TaskPriority, TaskService};
use tokio::sync::oneshot;
use tokio::time::Instant;

fn service() -> TaskService {
    TaskService::builder()
        .handle(tokio::runtime::Handle::current())
        .blocking_threads(2)
        .build()
        .expect("service builds")
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
async fn spawn_completes_and_is_counted() {
    let svc = service();
    let (tx, rx) = oneshot::channel::<()>();
    let id = svc.spawn_named("spawn-completes", async move {
        let _ = tx.send(());
    });
    rx.await.expect("task signalled");
    wait_until(|| svc.stats().completed == 1, Duration::from_secs(2)).await;
    let stats = svc.stats();
    assert_eq!(stats.completed, 1);
    assert_eq!(stats.active, 0);
    assert!(!stats.by_name.contains_key("spawn-completes"));
    // a finished task is gone from the registry; cancel reports false
    assert!(!svc.cancel(id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_tasks_run_on_the_pool() {
    let svc = service();
    let counter = Arc::new(AtomicUsize::new(0));
    for _ in 0..3 {
        let counter = counter.clone();
        svc.spawn_blocking("blocking-job", move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
    }
    wait_until(
        || counter.load(Ordering::SeqCst) == 3,
        Duration::from_secs(2),
    )
    .await;
    wait_until(|| svc.stats().completed == 3, Duration::from_secs(2)).await;
    assert_eq!(svc.stats().completed, 3);
    assert_eq!(svc.stats().active, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delay_fires_only_after_the_duration() {
    let svc = service();
    let start = Instant::now();
    let (tx, rx) = oneshot::channel::<Instant>();
    svc.delay("delayed", Duration::from_millis(10), async move {
        let _ = tx.send(Instant::now());
    });
    let fired = rx.await.expect("delay fired");
    assert!(
        fired.duration_since(start) >= Duration::from_millis(10),
        "delay fired too early"
    );
    wait_until(|| svc.stats().completed == 1, Duration::from_secs(2)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_repeats_then_cancel_stops_it() {
    let svc = service();
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_in_factory = counter.clone();
    let id = svc.every("every-tick", Duration::from_millis(40), move || {
        let counter = counter_in_factory.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });
    wait_until(
        || counter.load(Ordering::SeqCst) >= 3,
        Duration::from_secs(2),
    )
    .await;
    assert!(svc.cancel(id));
    let after_cancel = counter.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(counter.load(Ordering::SeqCst), after_cancel);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_waits_for_active_tasks() {
    let svc = service();
    let (tx, rx) = oneshot::channel::<()>();
    svc.spawn_named("drain-target", async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        let _ = tx.send(());
    });
    let start = Instant::now();
    let report = svc.drain(Duration::from_secs(2)).await;
    assert!(
        start.elapsed() >= Duration::from_millis(110),
        "drain returned before the task finished"
    );
    assert_eq!(report.completed, 1);
    assert_eq!(report.timed_out, 0);
    rx.await.expect("task completed during drain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_task_is_contained_and_service_survives() {
    let svc = service();
    svc.spawn_named("panic-me", async {
        panic!("boom");
    });
    let (tx, rx) = oneshot::channel::<()>();
    svc.spawn_named("stable-task", async move {
        let _ = tx.send(());
    });
    rx.await.expect("sibling finished");
    wait_until(|| svc.stats().panicked == 1, Duration::from_secs(2)).await;
    wait_until(|| svc.stats().completed == 1, Duration::from_secs(2)).await;
    // the service is still usable afterwards
    svc.spawn_named("after-panic", async {});
    let _ = svc.drain(Duration::from_secs(2)).await;
    assert_eq!(svc.stats().completed, 2, "post-panic tasks still run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_quota_rejects_at_max_concurrent() {
    let svc = service();
    let limits = ScopeLimits {
        max_concurrent: 1,
        max_queued: 0,
        ..ScopeLimits::default()
    };
    let scope = svc.scoped("quota-plug", limits);
    scope
        .submit("first", async {
            tokio::time::sleep(Duration::from_millis(250)).await;
        })
        .expect("first task accepted");
    let err = scope
        .submit("second", async {})
        .expect_err("second task exceeds the quota");
    match err {
        ScopeError::QuotaExceeded {
            running,
            running_max,
            queued,
            queued_max,
        } => {
            assert_eq!(running, 1);
            assert_eq!(running_max, 1);
            assert_eq!(queued, 0);
            assert_eq!(queued_max, 0);
        }
        other => panic!("expected QuotaExceeded, got {other:?}"),
    }
    let _ = svc.drain(Duration::from_secs(2)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_queued_task_runs_when_a_slot_frees() {
    let svc = service();
    let limits = ScopeLimits {
        max_concurrent: 1,
        max_queued: 1,
        ..ScopeLimits::default()
    };
    let scope = svc.scoped("queued-plug", limits);
    let hold = Arc::new(tokio::sync::Notify::new());
    let hold_in_first = hold.clone();
    scope
        .submit("first", async move {
            hold_in_first.notified().await;
        })
        .expect("first task accepted");
    let (tx, rx) = oneshot::channel::<()>();
    scope
        .submit("second", async move {
            let _ = tx.send(());
        })
        .expect("second task queued");
    assert_eq!(scope.stats().queued, 1);
    // third task overflows the 1-deep queue
    let err = scope.submit("third", async {}).expect_err("queue is full");
    assert!(matches!(err, ScopeError::QuotaExceeded { .. }));
    hold.notify_one();
    rx.await.expect("queued task ran after the slot freed");
    let _ = svc.drain(Duration::from_secs(2)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_task_times_out() {
    let svc = service();
    let limits = ScopeLimits {
        task_timeout: Duration::from_millis(100),
        ..ScopeLimits::default()
    };
    let scope = svc.scoped("timeout-plug", limits);
    scope
        .submit("slow", async {
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
        .expect("task accepted");
    wait_until(|| scope.stats().timed_out == 1, Duration::from_secs(2)).await;
    assert_eq!(scope.stats().active, 0);
    assert_eq!(svc.stats().timed_out, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_close_cancels_running_tasks() {
    let svc = service();
    let scope = svc.scoped("close-plug", ScopeLimits::default());
    scope
        .submit("pending", std::future::pending::<()>())
        .expect("task accepted");
    wait_until(|| scope.stats().active == 1, Duration::from_secs(2)).await;
    scope.close();
    assert_eq!(scope.stats().active, 0);
    assert_eq!(scope.stats().cancelled, 1);
    let err = scope
        .submit("after-close", async {})
        .expect_err("scope is closed");
    assert!(matches!(err, ScopeError::Closed(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stats_track_by_name() {
    let svc = service();
    let id = svc.spawn_named("named-task", std::future::pending::<()>());
    wait_until(|| svc.stats().active == 1, Duration::from_secs(2)).await;
    let stats = svc.stats();
    assert_eq!(stats.by_name.get("named-task"), Some(&1));
    assert_eq!(stats.by_priority.get(&TaskPriority::Normal), Some(&1));
    assert!(svc.cancel(id));
    wait_until(|| svc.stats().active == 0, Duration::from_secs(2)).await;
    assert!(!svc.stats().by_name.contains_key("named-task"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cron_registers_and_cancels() {
    let svc = service();
    let err = svc.cron("bad", "not a cron", || async {}).await;
    assert!(matches!(err, Err(ScheduleError::InvalidCron(_))));
    let id = svc
        .cron("cron-task", "0 0 0 1 1 *", || async {})
        .await
        .expect("valid schedule registers");
    assert_eq!(svc.stats().active, 1);
    assert_eq!(svc.stats().by_name.get("cron-task"), Some(&1));
    assert!(svc.cancel(id));
    assert_eq!(svc.stats().active, 0);
    assert!(!svc.cancel(id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_cancels_perpetual_watchers_instead_of_waiting() {
    let svc = service();
    // a perpetual interval watcher can never finish on its own; drain must
    // not sit on it until the deadline
    svc.every("perpetual", Duration::from_millis(10), || async {});
    let start = Instant::now();
    let report = svc.drain(Duration::from_secs(10)).await;
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "drain waited on a perpetual watcher"
    );
    assert_eq!(report.completed, 0);
    assert_eq!(report.cancelled, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_drain_spawns_are_cancelled_immediately() {
    let svc = service();
    let _ = svc.drain(Duration::from_secs(2)).await;
    let ran = Arc::new(AtomicUsize::new(0));
    let ran_in_task = ran.clone();
    let id = svc.spawn_named("post-drain", async move {
        ran_in_task.fetch_add(1, Ordering::SeqCst);
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "post-drain task must not execute"
    );
    assert!(!svc.cancel(id));
    assert_eq!(svc.stats().cancelled, 1);
}
