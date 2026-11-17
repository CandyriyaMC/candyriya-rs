//! Behavior tests for the supervision tree.
//!
//! No network, no sleeps longer than strictly necessary; every wait on the
//! implementation is a bounded poll, so the suite is deterministic on
//! loaded machines. The task service is real; nothing here fakes execution.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use candyriya_init::{
    ExitStatus, Init, InitError, InitOptions, Pid, ReapOutcome, RestartLimits, RestartPolicy,
    UnitInfo, UnitKind, UnitSpec, UnitState,
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

fn fast_restart_limits() -> RestartLimits {
    RestartLimits {
        base: Duration::from_millis(10),
        cap: Duration::from_millis(40),
        max_attempts: 5,
        window: Duration::from_secs(60),
        jitter: 0.0,
    }
}

fn fast_tree(limits: RestartLimits) -> Init {
    Init::with_options(
        service(),
        InitOptions {
            exit_ring_capacity: 64,
            stuck_threshold: Duration::from_millis(50),
            restart_limits: limits,
        },
    )
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

fn unit(tree: &Init, pid: Pid) -> UnitInfo {
    tree.unit(pid)
        .unwrap_or_else(|| panic!("unit {pid} should exist"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_exists_at_pid_one() {
    let tree = tree();
    assert_eq!(tree.root_pid(), Pid::INIT);
    let root = unit(&tree, Pid::INIT);
    assert_eq!(root.name, "init");
    assert_eq!(root.kind, UnitKind::LongLived);
    assert_eq!(root.state, UnitState::Running);
    assert_eq!(root.ppid, Pid::NONE);
    let snapshot = tree.snapshot();
    assert_eq!(snapshot.units, 1);
    assert_eq!(snapshot.running, 1);
    // the root never registers a task with the service
    assert_eq!(tree.service().stats().active, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_unit_runs_through_the_task_service() {
    let tree = tree();
    let ran = Arc::new(AtomicUsize::new(0));
    let ran_in_body = ran.clone();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "proof-of-service",
                UnitKind::Oneshot,
                RestartPolicy::Never,
                move || {
                    let ran = ran_in_body.clone();
                    async move {
                        ran.fetch_add(1, Ordering::SeqCst);
                        candyriya_init::ExitStatus::Success
                    }
                },
            ),
        )
        .expect("unit spawns");
    wait_until(|| ran.load(Ordering::SeqCst) == 1, Duration::from_secs(2)).await;
    wait_until(
        || tree.service().stats().completed == 1,
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(tree.snapshot().units, 2);
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    let info = unit(&tree, pid);
    assert_eq!(info.state, UnitState::Zombie);
    assert_eq!(info.ppid, Pid::INIT);
    assert!(matches!(
        info.last_exit,
        Some(candyriya_init::ExitStatus::Success)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_rejects_unknown_parent() {
    let tree = tree();
    // pids are never reused and never re-allocated after a reap, so a
    // reaped pid stays absent forever and names a valid "no such parent"
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new("gone", UnitKind::Oneshot, RestartPolicy::Never, || async {
                candyriya_init::ExitStatus::Success
            }),
        )
        .expect("unit spawns");
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    tree.reap(pid).expect("unit reaps");
    let err = tree.spawn_unit(
        pid,
        UnitSpec::new(
            "orphan",
            UnitKind::Oneshot,
            RestartPolicy::Never,
            || async { candyriya_init::ExitStatus::Success },
        ),
    );
    assert!(matches!(err, Err(InitError::NoSuchParent(p)) if p == pid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_rejects_zombie_parent() {
    let tree = tree();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "short-lived-parent",
                UnitKind::Oneshot,
                RestartPolicy::Never,
                || async { candyriya_init::ExitStatus::Success },
            ),
        )
        .expect("unit spawns");
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    // parent exists but is a zombie: cannot supervise a child
    let err = tree.spawn_unit(
        pid,
        UnitSpec::new("child", UnitKind::Oneshot, RestartPolicy::Never, || async {
            candyriya_init::ExitStatus::Success
        }),
    );
    assert!(matches!(err, Err(InitError::ParentNotRunning(p)) if p == pid));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_validates_name() {
    let tree = tree();
    for (name, why) in [("", "empty"), ("valid-name_1.2:ok", "ok")] {
        let spec = UnitSpec::new(name, UnitKind::Oneshot, RestartPolicy::Never, || async {
            candyriya_init::ExitStatus::Success
        });
        assert_eq!(tree.spawn_unit(Pid::INIT, spec).is_ok(), why == "ok");
    }
    let long = "x".repeat(129);
    let spec = UnitSpec::new(long, UnitKind::Oneshot, RestartPolicy::Never, || async {
        candyriya_init::ExitStatus::Success
    });
    assert!(matches!(
        tree.spawn_unit(Pid::INIT, spec),
        Err(InitError::InvalidName(_))
    ));
    let control = "bad\nname";
    let spec = UnitSpec::new(control, UnitKind::Oneshot, RestartPolicy::Never, || async {
        candyriya_init::ExitStatus::Success
    });
    assert!(matches!(
        tree.spawn_unit(Pid::INIT, spec),
        Err(InitError::InvalidName(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn success_exit_leads_to_zombie_and_reap() {
    let tree = tree();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "exits-ok",
                UnitKind::Oneshot,
                RestartPolicy::Never,
                || async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    candyriya_init::ExitStatus::Success
                },
            ),
        )
        .expect("unit spawns");
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    let outcome = tree.reap(pid).expect("zombie reaps");
    let ReapOutcome::Reaped(record) = outcome else {
        panic!("expected Reaped, got {outcome:?}")
    };
    assert_eq!(record.pid, pid);
    assert_eq!(record.ppid, Pid::INIT);
    assert_eq!(record.name, "exits-ok");
    assert_eq!(record.status, candyriya_init::ExitStatus::Success);
    assert_eq!(record.restart_count, 0);
    assert!(tree.unit(pid).is_none(), "reaped unit leaves the registry");
    assert_eq!(tree.zombie_count(), 0);
    let snapshot = tree.snapshot();
    assert_eq!(snapshot.reaps_total, 1);
    assert_eq!(snapshot.exits_recorded_total, 1);
    // the exit landed in the ring
    let exits = tree.recent_exits(8);
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].pid, pid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failure_status_is_kept_bounded() {
    let tree = tree();
    let long_message = "boom".to_string() + &"x".repeat(4096);
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "exits-failure",
                UnitKind::Oneshot,
                RestartPolicy::Never,
                move || {
                    let message = long_message.clone();
                    async move { candyriya_init::ExitStatus::failure(message) }
                },
            ),
        )
        .expect("unit spawns");
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    let exits = tree.recent_exits(1);
    assert_eq!(exits.len(), 1);
    let candyriya_init::ExitStatus::Failure(message) = &exits[0].status else {
        panic!("expected Failure status")
    };
    assert!(message.len() <= candyriya_init::MAX_FAILURE_MESSAGE_BYTES);
    assert!(message.is_char_boundary(message.len()));
    let _ = tree.reap(pid).expect("zombie reaps");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_body_is_recorded_panicked() {
    let tree = tree();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "panics",
                UnitKind::Oneshot,
                RestartPolicy::Never,
                || async {
                    panic!("boom");
                },
            ),
        )
        .expect("unit spawns");
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    let outcome = tree.reap(pid).expect("zombie reaps");
    let ReapOutcome::Reaped(record) = outcome else {
        panic!("expected Reaped, got {outcome:?}")
    };
    assert_eq!(record.status, candyriya_init::ExitStatus::Panicked);
    // the service's own panic accounting observes the task asynchronously;
    // the tree's guard records the exit synchronously, so it may already know
    wait_until(
        || tree.service().stats().panicked >= 1,
        Duration::from_secs(2),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_body_is_recorded_cancelled() {
    let tree = tree();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "pending",
                UnitKind::LongLived,
                RestartPolicy::Always,
                || async { std::future::pending().await },
            ),
        )
        .expect("unit spawns");
    wait_until(
        || tree.service().stats().active == 1,
        Duration::from_secs(2),
    )
    .await;
    assert!(tree.cancel(pid).expect("unit exists"));
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    let outcome = tree.reap(pid).expect("zombie reaps");
    let ReapOutcome::Reaped(record) = outcome else {
        panic!("stop-requested unit must not be restarted, got {outcome:?}")
    };
    assert_eq!(record.status, candyriya_init::ExitStatus::Cancelled);
    // a requested stop suppresses restart even under the Always policy
    assert!(tree.unit(pid).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timed_out_body_is_recorded_timed_out() {
    let tree = tree();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new("slow", UnitKind::Oneshot, RestartPolicy::Never, || async {
                std::future::pending().await
            })
            .with_timeout(Duration::from_millis(50)),
        )
        .expect("unit spawns");
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    let outcome = tree.reap(pid).expect("zombie reaps");
    let ReapOutcome::Reaped(record) = outcome else {
        panic!("expected Reaped, got {outcome:?}")
    };
    assert_eq!(record.status, candyriya_init::ExitStatus::TimedOut);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphans_reparent_to_init_when_parent_exits() {
    let tree = tree();
    let parent = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "parent",
                UnitKind::LongLived,
                RestartPolicy::Never,
                || async { std::future::pending().await },
            ),
        )
        .expect("parent spawns");
    let child = tree
        .spawn_unit(
            parent,
            UnitSpec::new(
                "child",
                UnitKind::LongLived,
                RestartPolicy::Never,
                || async { std::future::pending().await },
            ),
        )
        .expect("child spawns");
    assert_eq!(unit(&tree, child).ppid, parent);
    assert!(tree.cancel(parent).expect("parent exists"));
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    // re-parenting happens at exit time, before anyone reaps
    assert_eq!(unit(&tree, child).ppid, Pid::INIT);
    // reaping from init harvests the parent; the child is untouched
    let outcomes = tree.reap_children(Pid::INIT);
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(&outcomes[0], ReapOutcome::Reaped(r) if r.pid == parent));
    assert_eq!(unit(&tree, child).state, UnitState::Running);
    // the child is now init's direct child and reaps through init
    assert!(tree.cancel(child).expect("child exists"));
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    let outcomes = tree.reap_children(Pid::INIT);
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(&outcomes[0], ReapOutcome::Reaped(r) if r.pid == child));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reap_of_running_unit_is_rejected() {
    let tree = tree();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "running",
                UnitKind::LongLived,
                RestartPolicy::Never,
                || async { std::future::pending().await },
            ),
        )
        .expect("unit spawns");
    let err = tree.reap(pid);
    assert!(matches!(
        err,
        Err(InitError::NotZombie {
            pid: p,
            state: UnitState::Running
        }) if p == pid
    ));
    // failed reap does not consume the unit
    assert_eq!(unit(&tree, pid).state, UnitState::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reap_children_reaps_only_zombies_in_order() {
    let tree = tree();
    for name in ["first", "second"] {
        tree.spawn_unit(
            Pid::INIT,
            UnitSpec::new(name, UnitKind::Oneshot, RestartPolicy::Never, || async {
                candyriya_init::ExitStatus::Success
            }),
        )
        .expect("unit spawns");
    }
    let pending = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "pending",
                UnitKind::LongLived,
                RestartPolicy::Never,
                || async { std::future::pending().await },
            ),
        )
        .expect("unit spawns");
    wait_until(|| tree.zombie_count() == 2, Duration::from_secs(2)).await;
    let outcomes = tree.reap_children(Pid::INIT);
    assert_eq!(outcomes.len(), 2);
    let pids: Vec<u64> = outcomes
        .iter()
        .map(|o| match o {
            ReapOutcome::Reaped(r) => r.pid.as_u64(),
            other => panic!("expected Reaped, got {other:?}"),
        })
        .collect();
    assert!(
        pids.windows(2).all(|w| w[0] < w[1]),
        "reaps happen in pid order"
    );
    assert_eq!(unit(&tree, pending).state, UnitState::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_failure_unit_restarts_in_place_with_backoff() {
    let tree = fast_tree(fast_restart_limits());
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_in_factory = calls.clone();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "flaky",
                UnitKind::LongLived,
                RestartPolicy::OnFailure,
                move || {
                    let call = calls_in_factory.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if call == 0 {
                            candyriya_init::ExitStatus::failure("first run fails")
                        } else {
                            std::future::pending().await
                        }
                    }
                },
            ),
        )
        .expect("unit spawns");
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    let outcome = tree.reap(pid).expect("failed unit reaps");
    let ReapOutcome::Restarting { record, after } = outcome else {
        panic!("expected Restarting, got {outcome:?}")
    };
    assert_eq!(
        record.status,
        candyriya_init::ExitStatus::failure("first run fails")
    );
    assert!(
        after >= Duration::from_millis(7),
        "jittered 10ms base, got {after:?}"
    );
    // same pid across the restart
    wait_until(
        || unit(&tree, pid).restart_count == 1,
        Duration::from_secs(2),
    )
    .await;
    let info = unit(&tree, pid);
    assert_eq!(info.name, "flaky");
    // clean up
    assert!(tree.cancel(pid).expect("unit exists"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn always_restarts_even_on_success() {
    let tree = fast_tree(fast_restart_limits());
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "heartbeat",
                UnitKind::Every,
                RestartPolicy::Always,
                || async { candyriya_init::ExitStatus::Success },
            ),
        )
        .expect("unit spawns");
    for restarts in 1..=3 {
        wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
        let outcome = tree.reap(pid).expect("zombie reaps");
        assert!(
            matches!(outcome, ReapOutcome::Restarting { .. }),
            "Always policy restarts even on success, got {outcome:?}"
        );
        // restart_count bumps at respawn and persists through the transient
        // Running state, so it is the stable thing to poll for
        wait_until(
            || unit(&tree, pid).restart_count == restarts,
            Duration::from_secs(2),
        )
        .await;
    }
    let _ = tree.cancel(pid).expect("unit exists");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_start_limit_moves_unit_to_failed_terminal() {
    let tree = fast_tree(RestartLimits {
        base: Duration::from_millis(5),
        cap: Duration::from_millis(20),
        max_attempts: 3,
        window: Duration::from_secs(60),
        jitter: 0.0,
    });
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "crashlooping",
                UnitKind::LongLived,
                RestartPolicy::OnFailure,
                || async { candyriya_init::ExitStatus::failure("always fails") },
            ),
        )
        .expect("unit spawns");
    // drive the crash loop: each iteration reaps one zombie; the fourth
    // reap blows the 3-attempts-in-window budget
    for expected_restarts in 1..=3 {
        wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
        let outcome = tree.reap(pid).expect("zombie reaps");
        assert!(
            matches!(outcome, ReapOutcome::Restarting { .. }),
            "expected restart #{expected_restarts}, got {outcome:?}"
        );
        wait_until(
            || unit(&tree, pid).restart_count == expected_restarts,
            Duration::from_secs(2),
        )
        .await;
    }
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    let outcome = tree.reap(pid).expect("zombie reaps");
    let ReapOutcome::FailedTerminal { record } = outcome else {
        panic!("expected FailedTerminal, got {outcome:?}")
    };
    assert_eq!(
        record.status,
        candyriya_init::ExitStatus::failure("always fails")
    );
    assert_eq!(record.restart_count, 3);
    let info = unit(&tree, pid);
    assert_eq!(info.state, UnitState::FailedTerminal);
    assert!(matches!(
        info.last_exit,
        Some(candyriya_init::ExitStatus::Failure(_))
    ));
    let snapshot = tree.snapshot();
    assert_eq!(snapshot.failed_terminal, 1);
    assert_eq!(snapshot.failed_terminal_total, 1);
    assert_eq!(snapshot.restarts_total, 3);
    assert_eq!(snapshot.exits_recorded_total, 4);
    // terminal units are not zombies; reaping is rejected
    assert!(matches!(
        tree.reap(pid),
        Err(InitError::NotZombie {
            state: UnitState::FailedTerminal,
            ..
        })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zombie_ring_is_bounded_and_reports_dropped_records() {
    let tree = Init::with_options(
        service(),
        InitOptions {
            exit_ring_capacity: 3,
            stuck_threshold: Duration::from_secs(30),
            restart_limits: fast_restart_limits(),
        },
    );
    let mut spawned = Vec::new();
    for i in 0..5 {
        spawned.push(
            tree.spawn_unit(
                Pid::INIT,
                UnitSpec::new(
                    format!("ring-{i}"),
                    UnitKind::Oneshot,
                    RestartPolicy::Never,
                    || async { candyriya_init::ExitStatus::Success },
                ),
            )
            .expect("unit spawns"),
        );
    }
    wait_until(|| tree.zombie_count() == 5, Duration::from_secs(2)).await;
    let outcomes = tree.reap_children(Pid::INIT);
    assert_eq!(outcomes.len(), 5);
    let exits = tree.recent_exits(10);
    // which exits were evicted is up to scheduling; the bound itself is not
    assert_eq!(exits.len(), 3, "ring keeps exactly its capacity");
    let names: Vec<&str> = exits.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.iter().all(|name| name.starts_with("ring-")),
        "entries come from the spawned units: {names:?}"
    );
    let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
    assert_eq!(unique.len(), 3, "no duplicate entries in the ring");
    // the ring holds exits in the order they were recorded
    for pair in exits.windows(2) {
        assert!(pair[0].exited_at <= pair[1].exited_at);
    }
    let snapshot = tree.snapshot();
    assert_eq!(snapshot.exits_recorded_total, 5);
    assert_eq!(snapshot.exit_ring_dropped, 2);
    assert_eq!(snapshot.exit_ring_capacity, 3);
    let _ = spawned;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_zombies_are_reported_after_threshold() {
    let tree = fast_tree(fast_restart_limits());
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "forgotten",
                UnitKind::Oneshot,
                RestartPolicy::Never,
                || async { candyriya_init::ExitStatus::Success },
            ),
        )
        .expect("unit spawns");
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    // nothing is stuck yet — the zombie is fresh
    assert!(tree.stuck_zombies().is_empty());
    tokio::time::sleep(Duration::from_millis(75)).await;
    let stuck = tree.stuck_zombies();
    assert_eq!(stuck.len(), 1);
    assert_eq!(stuck[0].pid, pid);
    assert_eq!(tree.snapshot().stuck_zombies, 1);
    // reaping clears the count
    tree.reap(pid).expect("zombie reaps");
    assert!(tree.stuck_zombies().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_on_init_reports_false_and_stays_running() {
    let tree = tree();
    assert!(!tree.cancel(Pid::INIT).expect("init exists"));
    assert_eq!(unit(&tree, Pid::INIT).state, UnitState::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reaped_exit_record_matches_ring_record() {
    let tree = tree();
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "ring-consistency",
                UnitKind::Oneshot,
                RestartPolicy::Never,
                || async { candyriya_init::ExitStatus::Success },
            ),
        )
        .expect("unit spawns");
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    let outcome = tree.reap(pid).expect("zombie reaps");
    let ReapOutcome::Reaped(record) = outcome else {
        panic!("expected Reaped, got {outcome:?}")
    };
    let ring = tree.recent_exits(1);
    assert_eq!(ring.len(), 1);
    assert_eq!(
        ring[0], record,
        "reap returns the same record the ring keeps"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_during_backoff_folds_into_plain_removal() {
    let tree = fast_tree(RestartLimits {
        base: Duration::from_millis(200), // long enough to stop it mid-backoff
        cap: Duration::from_millis(200),
        max_attempts: 5,
        window: Duration::from_secs(60),
        jitter: 0.0,
    });
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "backoff-stop",
                UnitKind::LongLived,
                RestartPolicy::Always,
                || async { candyriya_init::ExitStatus::failure("fails fast") },
            ),
        )
        .expect("unit spawns");
    wait_until(|| tree.zombie_count() == 1, Duration::from_secs(2)).await;
    let outcome = tree.reap(pid).expect("zombie reaps");
    assert!(matches!(outcome, ReapOutcome::Restarting { .. }));
    assert_eq!(unit(&tree, pid).state, UnitState::Restarting);
    // a stop requested during the backoff is honored when the timer fires
    let _ = tree.cancel(pid).expect("unit exists");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        tree.unit(pid).is_none(),
        "the pending restart must fold into a removal, not respawn the unit"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopted_unit_is_born_zombie_then_reaped_with_outcome() {
    let tree = tree();
    let ok_pid = tree
        .adopt_unit(
            Pid::INIT,
            "bootstrap.ok",
            UnitKind::Oneshot,
            ExitStatus::Success,
        )
        .expect("adoption succeeds");
    let failed_pid = tree
        .adopt_unit(
            Pid::INIT,
            "bootstrap.failed",
            UnitKind::Oneshot,
            ExitStatus::failure("the loader step did not finish"),
        )
        .expect("adoption succeeds");
    // both are visible as zombies before anyone harvests them, each with
    // its own outcome, and neither ran against a task service slot
    for (pid, status) in [
        (ok_pid, ExitStatus::Success),
        (
            failed_pid,
            ExitStatus::failure("the loader step did not finish"),
        ),
    ] {
        let info = unit(&tree, pid);
        assert_eq!(info.state, UnitState::Zombie);
        assert_eq!(info.last_exit.as_ref(), Some(&status));
        assert_eq!(info.restart_count, 0);
        assert_eq!(info.ppid, Pid::INIT);
        assert_eq!(info.kind, UnitKind::Oneshot);
    }
    let outcomes = tree.reap_children(Pid::INIT);
    assert_eq!(outcomes.len(), 2);
    let mut by_pid: std::collections::HashMap<_, _> = outcomes
        .into_iter()
        .map(|outcome| {
            let ReapOutcome::Reaped(record) = outcome else {
                panic!("an adopted unit is never re-run or failed terminally, got {outcome:?}")
            };
            (record.pid, record)
        })
        .collect();
    assert_eq!(
        by_pid.remove(&ok_pid).map(|record| record.status),
        Some(ExitStatus::Success)
    );
    assert_eq!(
        by_pid.remove(&failed_pid).map(|record| record.status),
        Some(ExitStatus::failure("the loader step did not finish"))
    );
    // reaped means gone from the registry; the ring keeps both exits
    assert!(tree.unit(ok_pid).is_none());
    assert!(tree.unit(failed_pid).is_none());
    assert_eq!(tree.zombie_count(), 0);
    let ring = tree.recent_exits(2);
    assert_eq!(ring.len(), 2);
    assert!(ring.iter().any(|record| record.pid == ok_pid));
    assert!(ring.iter().any(|record| record.pid == failed_pid));
}
