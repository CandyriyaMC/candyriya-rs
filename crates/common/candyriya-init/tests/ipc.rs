//! End-to-end tests for the capability-IPC plane.
//!
//! No network, no sleeps longer than strictly necessary; every wait on the
//! implementation is a bounded poll, so the suite is deterministic on
//! loaded machines. The task service and the tree are real.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use candyriya_init::{
    CallError, CallOptions, Init, InitError, InitOptions, Pid, ReapOutcome, RestartLimits,
    RestartPolicy, ServiceKind, UnitKind, UnitSpec, UnitState, current_unit,
};
use candyriya_scheduler::TaskService;
use tokio::sync::Notify;
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

fn fast_tree() -> Init {
    Init::with_options(
        service(),
        InitOptions {
            exit_ring_capacity: 64,
            stuck_threshold: Duration::from_millis(50),
            restart_limits: RestartLimits {
                base: Duration::from_millis(10),
                cap: Duration::from_millis(40),
                max_attempts: 5,
                window: Duration::from_secs(60),
                jitter: 0.0,
            },
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

// request/response pair used by the tests; services declare their own
// message types on purpose, exercising the per-service enum model
#[derive(Debug)]
enum ConfigRequest {
    Get,
}

#[derive(Debug, PartialEq, Eq)]
struct ConfigValue(u64);

/// A service unit that answers instantly.
fn config_spec() -> UnitSpec {
    UnitSpec::service(
        "config",
        UnitKind::LongLived,
        RestartPolicy::Never,
        |_ctx, msg: ConfigRequest| async move {
            match msg {
                ConfigRequest::Get => ConfigValue(41),
            }
        },
    )
    .with_service_kind(ServiceKind::Config)
}

/// A service unit whose handler pends forever (for timeout/busy/cancel tests).
fn pending_spec(name: &str) -> UnitSpec {
    UnitSpec::service(
        name,
        UnitKind::LongLived,
        RestartPolicy::Never,
        |_ctx, msg: ConfigRequest| async move {
            match msg {
                ConfigRequest::Get => std::future::pending::<ConfigValue>().await,
            }
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ancestor_calls_child_and_grandchild() {
    let tree = tree();
    let child = tree
        .spawn_unit(Pid::INIT, config_spec())
        .expect("child spawns");
    let grandchild = tree
        .spawn_unit(child, config_spec())
        .expect("grandchild spawns");

    // (i) ancestor -> descendant, one level
    let reply = tree
        .call::<ConfigRequest, ConfigValue>(Pid::INIT, child, ConfigRequest::Get)
        .await
        .expect("ancestor call succeeds");
    assert_eq!(reply, ConfigValue(41));

    // (i) ancestor -> descendant, two levels, skipping the middle unit
    let reply = tree
        .call::<ConfigRequest, ConfigValue>(Pid::INIT, grandchild, ConfigRequest::Get)
        .await
        .expect("grandchild call succeeds");
    assert_eq!(reply, ConfigValue(41));

    // and the middle unit can call its own child
    let reply = tree
        .call::<ConfigRequest, ConfigValue>(child, grandchild, ConfigRequest::Get)
        .await
        .expect("parent-to-child call succeeds");
    assert_eq!(reply, ConfigValue(41));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sibling_cannot_call_sibling_without_grant() {
    let tree = tree();
    let left = tree
        .spawn_unit(Pid::INIT, config_spec())
        .expect("left spawns");
    let right = tree
        .spawn_unit(Pid::INIT, config_spec())
        .expect("right spawns");

    // (ii) no grant: rejected by the capability check, deterministically
    let err = tree
        .call::<ConfigRequest, ConfigValue>(left, right, ConfigRequest::Get)
        .await
        .expect_err("ungranted sibling call is denied");
    assert_eq!(err, CallError::AccessDenied);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_flows_down_and_regrant_only_via_holder() {
    let tree = tree();
    let cache = tree
        .spawn_unit(Pid::INIT, config_spec())
        .expect("cache spawns");

    // (iii) the target's parent grants a handle at spawn time, like an fd
    let a = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new("a", UnitKind::LongLived, RestartPolicy::Never, || async {
                std::future::pending::<candyriya_init::ExitStatus>().await
            })
            .with_grant(cache),
        )
        .expect("a spawns with a grant");

    // the holder can call the granted target
    let reply = tree
        .call::<ConfigRequest, ConfigValue>(a, cache, ConfigRequest::Get)
        .await
        .expect("granted call succeeds");
    assert_eq!(reply, ConfigValue(41));

    // the grant passes down one level: a spawns a2 with the same handle.
    // a is not an ancestor of cache; a holds the grant, so passing down is legal.
    let a2 = tree
        .spawn_unit(
            a,
            UnitSpec::new("a2", UnitKind::LongLived, RestartPolicy::Never, || async {
                std::future::pending::<candyriya_init::ExitStatus>().await
            })
            .with_grant(cache),
        )
        .expect("a2 spawns with the re-granted handle");
    let reply = tree
        .call::<ConfigRequest, ConfigValue>(a2, cache, ConfigRequest::Get)
        .await
        .expect("re-granted call succeeds");
    assert_eq!(reply, ConfigValue(41));

    // and one further level, through the holder's holder
    let a3 = tree
        .spawn_unit(
            a2,
            UnitSpec::new("a3", UnitKind::LongLived, RestartPolicy::Never, || async {
                std::future::pending::<candyriya_init::ExitStatus>().await
            })
            .with_grant(cache),
        )
        .expect("a3 spawns via a2's grant");
    assert!(
        tree.call::<ConfigRequest, ConfigValue>(a3, cache, ConfigRequest::Get)
            .await
            .is_ok()
    );

    // a unit can never mint a handle it doesn't hold:
    // b is an ungranted sibling of a, so b cannot pass cache to its child
    let b = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new("b", UnitKind::LongLived, RestartPolicy::Never, || async {
                std::future::pending::<candyriya_init::ExitStatus>().await
            }),
        )
        .expect("b spawns");
    let denied = tree.spawn_unit(
        b,
        UnitSpec::new("b2", UnitKind::LongLived, RestartPolicy::Never, || async {
            std::future::pending::<candyriya_init::ExitStatus>().await
        })
        .with_grant(cache),
    );
    assert!(
        matches!(denied, Err(InitError::GrantNotHeld { parent, target })
            if parent == b && target == cache),
        "a non-holder must not pass a handle down: {denied:?}"
    );

    // and b itself cannot call cache
    let err = tree
        .call::<ConfigRequest, ConfigValue>(b, cache, ConfigRequest::Get)
        .await
        .expect_err("ungranted call is denied");
    assert_eq!(err, CallError::AccessDenied);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_to_reaped_target_fails_the_spawn_untouched() {
    let tree = tree();
    let oneshot = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "oneshot",
                UnitKind::Oneshot,
                RestartPolicy::Never,
                || async { candyriya_init::ExitStatus::Success },
            ),
        )
        .expect("oneshot spawns");
    wait_until(
        || tree.unit(oneshot).map(|u| u.state) == Some(UnitState::Zombie),
        Duration::from_secs(2),
    )
    .await;
    tree.reap(oneshot).expect("reap succeeds");

    let before = tree.units().len();
    let denied = tree.spawn_unit(
        Pid::INIT,
        UnitSpec::new(
            "with-ghost-grant",
            UnitKind::LongLived,
            RestartPolicy::Never,
            || async { candyriya_init::ExitStatus::Success },
        )
        .with_grant(oneshot),
    );
    assert!(
        matches!(denied, Err(InitError::NoSuchUnit(pid)) if pid == oneshot),
        "grant to a reaped unit fails the spawn: {denied:?}"
    );
    assert_eq!(tree.units().len(), before, "failed spawn must not register");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_times_out_when_handler_never_answers() {
    let tree = tree();
    let slow = tree
        .spawn_unit(Pid::INIT, pending_spec("slow"))
        .expect("slow service spawns");

    // (iv) per-call timeout
    let err = tree
        .call_with_options::<ConfigRequest, ConfigValue>(
            Pid::INIT,
            slow,
            ConfigRequest::Get,
            CallOptions::timeout(Duration::from_millis(50)),
        )
        .await
        .expect_err("the call must time out");
    assert!(
        matches!(err, CallError::Timeout { after } if after == Duration::from_millis(50)),
        "expected a timeout, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_unit_call_timeout_applies_when_no_per_call_override() {
    let tree = tree();
    // the caller carries the knob and is the target's ancestor, so the call
    // is authorized; the timeout applies to calls IT makes
    let caller = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "caller",
                UnitKind::LongLived,
                RestartPolicy::Never,
                || async { std::future::pending::<candyriya_init::ExitStatus>().await },
            )
            .with_call_timeout(Duration::from_millis(30)),
        )
        .expect("caller spawns");
    let slow = tree
        .spawn_unit(caller, pending_spec("slow"))
        .expect("slow service spawns");

    let started = Instant::now();
    let err = tree
        .call::<ConfigRequest, ConfigValue>(caller, slow, ConfigRequest::Get)
        .await
        .expect_err("per-unit call timeout must apply");
    assert!(
        matches!(err, CallError::Timeout { after } if after == Duration::from_millis(30)),
        "expected the caller's per-unit timeout, got {err:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_mailbox_rejects_with_busy() {
    let tree = tree();
    let entered = Arc::new(AtomicBool::new(false));
    let entered_handler = Arc::clone(&entered);
    let blocker = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::service(
                "busy-service",
                UnitKind::LongLived,
                RestartPolicy::Never,
                move |_ctx, msg: ConfigRequest| {
                    let entered = Arc::clone(&entered_handler);
                    async move {
                        match msg {
                            ConfigRequest::Get => {
                                entered.store(true, Ordering::SeqCst);
                                std::future::pending::<ConfigValue>().await
                            }
                        }
                    }
                },
            )
            .with_mailbox_capacity(1),
        )
        .expect("busy service spawns");

    // first call occupies the handler; route it into the tree so it runs
    let tree2 = tree.clone();
    let bg = tree.service().spawn_named("test-busy-first", async move {
        let _ = tree2
            .call::<ConfigRequest, ConfigValue>(Pid::INIT, blocker, ConfigRequest::Get)
            .await;
    });
    wait_until(|| entered.load(Ordering::SeqCst), Duration::from_secs(2)).await;

    // second call lands in the 1-slot mailbox and stays queued there
    let tree3 = tree.clone();
    let bg2 = tree.service().spawn_named("test-busy-second", async move {
        let _ = tree3
            .call::<ConfigRequest, ConfigValue>(Pid::INIT, blocker, ConfigRequest::Get)
            .await;
    });

    // (v) once the only slot is taken, further calls are rejected with Busy.
    // The second call may take a scheduler beat to land in the mailbox, so
    // poll with a cheap rejected-or-queued probe: a queued probe call would
    // hang, so the probe itself races the mailbox with a short deadline.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        assert!(Instant::now() < deadline, "mailbox never reported full");
        let probe = tokio::time::timeout(
            Duration::from_millis(5),
            tree.call::<ConfigRequest, ConfigValue>(Pid::INIT, blocker, ConfigRequest::Get),
        )
        .await;
        match probe {
            Ok(Err(CallError::Busy)) => break,
            Ok(Ok(_)) => panic!("a third answer materialized out of nowhere"),
            Ok(Err(other)) => panic!("unexpected outcome: {other:?}"),
            // the probe went into the queue and is now waiting: the mailbox
            // holds it, the handler still never answers, drop it and retry —
            // the next probe must find the mailbox full
            Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }

    tree.service().cancel(bg);
    tree.service().cancel(bg2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_to_reaped_unit_fails_fast_with_not_running() {
    let tree = tree();
    let target = tree
        .spawn_unit(Pid::INIT, pending_spec("gone"))
        .expect("target spawns");

    tree.cancel(target).expect("cancel succeeds");
    wait_until(
        || tree.unit(target).map(|u| u.state) == Some(UnitState::Zombie),
        Duration::from_secs(2),
    )
    .await;
    match tree.reap(target).expect("reap succeeds") {
        ReapOutcome::Reaped(record) => assert_eq!(record.pid, target),
        other => panic!("expected a reaped outcome, got {other:?}"),
    }

    // (vi) already reaped: immediate error, not a hang
    let started = Instant::now();
    let err = tree
        .call::<ConfigRequest, ConfigValue>(Pid::INIT, target, ConfigRequest::Get)
        .await
        .expect_err("reaped target must not answer");
    assert_eq!(err, CallError::NotRunning);
    assert!(started.elapsed() < Duration::from_secs(1));

    // and a running plain unit (no service handler) is also NotRunning,
    // not a timeout — it owns a mailbox with no receiver
    let plain = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "plain",
                UnitKind::LongLived,
                RestartPolicy::Never,
                || async { std::future::pending::<candyriya_init::ExitStatus>().await },
            ),
        )
        .expect("plain unit spawns");
    let err = tree
        .call::<ConfigRequest, ConfigValue>(Pid::INIT, plain, ConfigRequest::Get)
        .await
        .expect_err("plain unit must reject calls");
    assert_eq!(err, CallError::NotRunning);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_panic_contains_and_caller_sees_panicked() {
    let tree = tree();
    let panicky = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::service(
                "panicky",
                UnitKind::LongLived,
                RestartPolicy::Never,
                |_ctx, msg: ConfigRequest| async move {
                    match msg {
                        ConfigRequest::Get => panic!("handler exploded"),
                    }
                },
            ),
        )
        .expect("panicky service spawns");

    // (vii) the caller gets the Panicked-class error, not a hang
    let err = tree
        .call::<ConfigRequest, ConfigValue>(Pid::INIT, panicky, ConfigRequest::Get)
        .await
        .expect_err("the handler panics");
    assert_eq!(err, CallError::Panicked);

    // and the unit's exit was contained and classified as Panicked
    wait_until(
        || tree.unit(panicky).map(|u| u.state) == Some(UnitState::Zombie),
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(
        tree.unit(panicky).expect("unit exists").last_exit,
        Some(candyriya_init::ExitStatus::Panicked)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn type_mismatch_is_an_error_never_a_panic() {
    let tree = tree();
    let config = tree
        .spawn_unit(Pid::INIT, config_spec())
        .expect("config spawns");

    #[derive(Debug)]
    struct WrongRequest;

    // (viii) callee downcast fails: error + metric, nobody panics
    let err = tree
        .call::<WrongRequest, ConfigValue>(Pid::INIT, config, WrongRequest)
        .await
        .expect_err("a wrong request type must mismatch");
    assert_eq!(err, CallError::TypeMismatch);

    // the service is still alive and answers correctly afterwards
    let reply = tree
        .call::<ConfigRequest, ConfigValue>(Pid::INIT, config, ConfigRequest::Get)
        .await
        .expect("service survives the mismatch");
    assert_eq!(reply, ConfigValue(41));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_emit_without_a_recorder_and_nothing_explodes() {
    // (ix) no metrics recorder is installed anywhere in this test binary;
    // every emission path (ok, not_cappable, timeout) must be a silent no-op
    let tree = tree();
    let config = tree
        .spawn_unit(Pid::INIT, config_spec())
        .expect("config spawns");

    let reply = tree
        .call::<ConfigRequest, ConfigValue>(Pid::INIT, config, ConfigRequest::Get)
        .await
        .expect("call succeeds");
    assert_eq!(reply, ConfigValue(41));

    let denied = tree
        .call::<ConfigRequest, ConfigValue>(config, Pid::INIT, ConfigRequest::Get)
        .await
        .expect_err("denied");
    assert_eq!(denied, CallError::AccessDenied);

    let slow = tree
        .spawn_unit(Pid::INIT, pending_spec("timed"))
        .expect("slow service spawns");
    let err = tree
        .call_with_options::<ConfigRequest, ConfigValue>(
            Pid::INIT,
            slow,
            ConfigRequest::Get,
            CallOptions::timeout(Duration::from_millis(10)),
        )
        .await
        .expect_err("slow handler must time out");
    assert!(matches!(err, CallError::Timeout { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_wait_does_not_hold_the_registry_lock() {
    // (x) while a call is parked waiting for a slow handler, the registry
    // must stay responsive: snapshot reads and spawns must return
    // immediately. Guards against holding the tree lock across the await.
    let tree = tree();
    let gate = Arc::new(Notify::new());
    let gate_handler = Arc::clone(&gate);
    let guarded = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::service(
                "guarded",
                UnitKind::LongLived,
                RestartPolicy::Never,
                move |_ctx, msg: ConfigRequest| {
                    let gate = Arc::clone(&gate_handler);
                    async move {
                        match msg {
                            ConfigRequest::Get => {
                                gate.notified().await;
                                ConfigValue(7)
                            }
                        }
                    }
                },
            ),
        )
        .expect("guarded service spawns");

    let tree2 = tree.clone();
    let bg = tree.service().spawn_named("test-lock-check", async move {
        let _ = tree2
            .call::<ConfigRequest, ConfigValue>(Pid::INIT, guarded, ConfigRequest::Get)
            .await;
    });

    // give the call a beat to reach the handler
    tokio::time::sleep(Duration::from_millis(20)).await;

    // the registry answers promptly while the call is parked
    let started = Instant::now();
    let units = tree.units();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "registry stalled under a parked call"
    );
    assert!(units.iter().any(|u| u.pid == guarded));

    // and so does a fresh spawn (which takes the same lock)
    let started = Instant::now();
    let extra = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new("extra", UnitKind::Oneshot, RestartPolicy::Never, || async {
                candyriya_init::ExitStatus::Success
            }),
        )
        .expect("spawn proceeds while a call is parked");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "spawn stalled under a parked call"
    );
    assert!(tree.unit(extra).is_some(), "extra unit is registered");

    gate.notify_one();
    tree.service().cancel(bg);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_mid_call_fails_the_call() {
    // a target stopped while answering drops its in-flight reply channel;
    // the caller observes Panicked, never a hang
    let tree = tree();
    let blocked = tree
        .spawn_unit(Pid::INIT, pending_spec("blocked"))
        .expect("blocked service spawns");

    let call = tree.call_with_options::<ConfigRequest, ConfigValue>(
        Pid::INIT,
        blocked,
        ConfigRequest::Get,
        CallOptions::timeout(Duration::from_secs(5)),
    );
    tokio::pin!(call);
    tokio::select! {
        _ = &mut call => panic!("the call returned before the target was stopped"),
        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
    }
    tree.cancel(blocked).expect("cancel succeeds");
    let err = call.await.expect_err("the call fails after the stop");
    assert_eq!(err, CallError::Panicked);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_unit_reports_the_bodys_pid_and_calls() {
    let tree = tree();
    let config = tree
        .spawn_unit(Pid::INIT, config_spec())
        .expect("config spawns");

    let seen_pid = Arc::new(AtomicU64::new(0));
    let seen_answer = Arc::new(AtomicBool::new(false));
    // the reporter is a sibling of config, so it needs a grant to be
    // authorized to call it — exactly the cross-subtree case grants exist for
    let report = tree
        .spawn_unit(Pid::INIT, {
            let seen_pid = Arc::clone(&seen_pid);
            let seen_answer = Arc::clone(&seen_answer);
            let init = tree.clone();
            UnitSpec::new(
                "reporter",
                UnitKind::Oneshot,
                RestartPolicy::Never,
                move || {
                    let seen_pid = Arc::clone(&seen_pid);
                    let seen_answer = Arc::clone(&seen_answer);
                    let init = init.clone();
                    async move {
                        let Some(me) = current_unit() else {
                            return candyriya_init::ExitStatus::Failure("no unit context".into());
                        };
                        seen_pid.store(me.as_u64(), Ordering::SeqCst);
                        match init
                            .call_current::<ConfigRequest, ConfigValue>(config, ConfigRequest::Get)
                            .await
                        {
                            Ok(ConfigValue(41)) => {
                                seen_answer.store(true, Ordering::SeqCst);
                                candyriya_init::ExitStatus::Success
                            }
                            Ok(_) => candyriya_init::ExitStatus::Failure("wrong value".into()),
                            Err(_) => candyriya_init::ExitStatus::Failure("call failed".into()),
                        }
                    }
                },
            )
            .with_grant(config)
        })
        .expect("reporter spawns");

    wait_until(
        || seen_pid.load(Ordering::SeqCst) != 0,
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(seen_pid.load(Ordering::SeqCst), report.as_u64());
    wait_until(
        || seen_answer.load(Ordering::SeqCst),
        Duration::from_secs(2),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grants_are_pruned_when_the_holder_is_reaped() {
    let tree = tree();
    let cache = tree
        .spawn_unit(Pid::INIT, config_spec())
        .expect("cache spawns");
    let holder = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "holder",
                UnitKind::LongLived,
                RestartPolicy::Never,
                || async { std::future::pending::<candyriya_init::ExitStatus>().await },
            )
            .with_grant(cache),
        )
        .expect("holder spawns with grant");
    assert_eq!(tree.grants(holder), vec![cache]);

    tree.cancel(holder).expect("cancel succeeds");
    wait_until(
        || tree.unit(holder).map(|u| u.state) == Some(UnitState::Zombie),
        Duration::from_secs(2),
    )
    .await;
    tree.reap(holder).expect("reap succeeds");

    // the pid is gone for good, so its grant rows must be pruned too
    assert_eq!(tree.grants(holder), Vec::<Pid>::new());

    // a ghost pid holds nothing: the capability check fails (not_cappable),
    // not a lookup of stale rows
    let err = tree
        .call::<ConfigRequest, ConfigValue>(holder, cache, ConfigRequest::Get)
        .await
        .expect_err("reaped holder holds no capability");
    assert_eq!(err, CallError::AccessDenied);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grants_are_pruned_when_the_target_is_reaped() {
    let tree = tree();
    let holder = tree.spawn_unit(
        Pid::INIT,
        UnitSpec::new(
            "holder",
            UnitKind::LongLived,
            RestartPolicy::Never,
            || async { std::future::pending::<candyriya_init::ExitStatus>().await },
        )
        .with_grant(Pid::INIT),
    );
    assert!(
        matches!(holder, Err(InitError::GrantNotHeld { .. })),
        "init itself cannot be granted to a child of init via the grant path"
    );

    // grant a real target, then reap the target, then see the row pruned
    let cache = tree
        .spawn_unit(Pid::INIT, config_spec())
        .expect("cache spawns");
    let holder = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::new(
                "holder2",
                UnitKind::LongLived,
                RestartPolicy::Never,
                || async { std::future::pending::<candyriya_init::ExitStatus>().await },
            )
            .with_grant(cache),
        )
        .expect("holder spawns");
    assert_eq!(tree.grants(holder), vec![cache]);

    tree.cancel(cache).expect("cancel succeeds");
    wait_until(
        || tree.unit(cache).map(|u| u.state) == Some(UnitState::Zombie),
        Duration::from_secs(2),
    )
    .await;
    tree.reap(cache).expect("reap succeeds");

    // the row pointing at the removed target is gone
    assert_eq!(tree.grants(holder), Vec::<Pid>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_call_is_allowed() {
    let tree = tree();
    let me = tree
        .spawn_unit(Pid::INIT, config_spec())
        .expect("service spawns");
    let reply = tree
        .call::<ConfigRequest, ConfigValue>(me, me, ConfigRequest::Get)
        .await
        .expect("self-call succeeds");
    assert_eq!(reply, ConfigValue(41));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_can_call_anything_in_its_tree() {
    let tree = tree();
    let a = tree.spawn_unit(Pid::INIT, config_spec()).expect("a spawns");
    let b = tree.spawn_unit(a, config_spec()).expect("b spawns");
    let reply = tree
        .call::<ConfigRequest, ConfigValue>(Pid::INIT, b, ConfigRequest::Get)
        .await
        .expect("init reaches the deepest leaf");
    assert_eq!(reply, ConfigValue(41));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_ctx_names_unit_and_caller() {
    use std::sync::Mutex as StdMutex;

    let tree = tree();
    let seen: Arc<StdMutex<Option<(Pid, Pid)>>> = Arc::new(StdMutex::new(None));
    let seen_cell = Arc::clone(&seen);
    let service = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::service(
                "ctx-service",
                UnitKind::LongLived,
                RestartPolicy::Never,
                move |ctx, msg: ConfigRequest| {
                    let seen_cell = Arc::clone(&seen_cell);
                    async move {
                        match msg {
                            ConfigRequest::Get => {
                                *seen_cell.lock().expect("lock") = Some((ctx.unit(), ctx.caller()));
                                ConfigValue(9)
                            }
                        }
                    }
                },
            ),
        )
        .expect("ctx service spawns");

    let reply = tree
        .call::<ConfigRequest, ConfigValue>(Pid::INIT, service, ConfigRequest::Get)
        .await
        .expect("call succeeds");
    assert_eq!(reply, ConfigValue(9));
    let (unit, caller) = seen.lock().expect("lock").expect("handler ran");
    assert_eq!(unit, service);
    assert_eq!(caller, Pid::INIT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_mailbox_is_rebuilt_across_restarts() {
    // the whole wiring-per-run design hinges on this: after a restart the
    // node must hold a fresh mailbox, and calls must reach the new body
    let tree = fast_tree();
    let calls = Arc::new(AtomicU64::new(0));
    let pid = tree
        .spawn_unit(
            Pid::INIT,
            UnitSpec::service(
                "flaky",
                UnitKind::LongLived,
                RestartPolicy::Always,
                move |_ctx, msg: ConfigRequest| {
                    let calls = Arc::clone(&calls);
                    async move {
                        match msg {
                            ConfigRequest::Get => {
                                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                                    panic!("first call explodes");
                                }
                                ConfigValue(99)
                            }
                        }
                    }
                },
            ),
        )
        .expect("flaky service spawns");

    let err = tree
        .call::<ConfigRequest, ConfigValue>(Pid::INIT, pid, ConfigRequest::Get)
        .await
        .expect_err("first call panics");
    assert_eq!(err, CallError::Panicked);

    wait_until(
        || tree.unit(pid).map(|u| u.state) == Some(UnitState::Zombie),
        Duration::from_secs(2),
    )
    .await;
    tree.reap(pid).expect("reap schedules the restart");
    wait_until(
        || tree.unit(pid).map(|u| u.restart_count) == Some(1),
        Duration::from_secs(2),
    )
    .await;

    // the restarted body owns a fresh mailbox; the call path works end to end
    let reply = tree
        .call::<ConfigRequest, ConfigValue>(Pid::INIT, pid, ConfigRequest::Get)
        .await
        .expect("restarted service answers");
    assert_eq!(reply, ConfigValue(99));
}
