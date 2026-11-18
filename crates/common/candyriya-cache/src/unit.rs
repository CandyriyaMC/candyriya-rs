//! The `cache` unit: the supervised owner of the live cache manager.
//!
//! Every runtime read or write of the core cache goes through this unit as a
//! typed IPC call; no subsystem talks to the [`CacheManager`] directly once
//! the tree is up. The composition root spawns the unit at boot with the
//! live [`CacheManager`], and the unit serves it until the process exits or
//! the tree restarts it. The favicon is the reference caller today; the
//! plugin path migrates over in its own slice.
//!
//! # Why the protocol lives here
//!
//! The message pair is the contract between the composition root (which
//! spawns the unit and attaches the handler) and its callers. It is
//! deliberately small: one request enum, one response enum, zero
//! serialization — the tree type-erases envelopes and downcasts on the
//! callee. This is internal wiring, not a stabilized public API; adding a
//! variant is an explicit, reviewable change.
//!
//! # Reply cost
//!
//! Values cross as owned `Vec<u8>` copies, exactly the shape cachegirl hands
//! out for cache entries; a favicon-sized payload is a few KiB, so the copy
//! is not the interesting cost. The namespace is resolved per message
//! (`cache/candyriya`, two registry lookups) so the handler always observes
//! the manager's real state, including a manager closed by the shutdown
//! finalizer.
//!
//! # Replace semantics
//!
//! cachegirl keys are unique per namespace; creating the same key twice is
//! rejected. The favicon re-stores its icon unconditionally (boot warm-up and
//! `/candyriya reload`), so [`CacheMsg::Insert`] removes any stale entry
//! before inserting — the remove-then-insert sequence the favicon performed
//! inline before, folded into the one message. Callers that want strict
//! insert-must-not-exist semantics do not have them; that is a new variant by
//! explicit change, not a silent side effect.
//!
//! # Namespace scope
//!
//! The handler talks to the core namespace (`cache/candyriya`) only, the
//! same one [`CacheManager::core_cache`] returns. Plugin namespaces are
//! deliberately out of reach of this protocol. Keys are validated with the
//! same rules cachegirl enforces on resource names; a violation surfaces as
//! [`CacheResp::Rejected`], never as a panic or a backend call.
//!
//! # Capability
//!
//! Units call [`Init::call`] with their own pid; the subtree rule alone
//! does not cover sibling calls, so a unit that reads the cache needs a
//! grant to this unit, installed at its spawn. The composition root grants
//! one to `proxy.accept`; per-connection tasks are not units yet and
//! attribute to [`Pid::INIT`], which is an ancestor of every unit, so the
//! subtree rule authorizes them. The boot-time favicon warm-up and the
//! reload hand-off (driven from the blocking console thread) attribute to
//! the root the same way.
//!
//! # Failure vocabulary
//!
//! Callers see the tree's [`CallError`](candyriya_init::CallError). A reply
//! is only ever produced by a running unit; a missing or crashed unit fails
//! fast with `NotRunning`, a live-but-slow one with `Timeout`. The unit is
//! spawned with [`RestartPolicy::Always`] — it fronts a disposable store,
//! the supervisor replaces it instead of leaving callers to degrade — but
//! the supervision window in between can refuse a call; every call site
//! decides for itself how to fail.

use std::sync::Arc;
use std::time::Duration;

use candyriya_init::{CallError, Init, Pid, RestartPolicy, ServiceKind, UnitKind, UnitSpec};

use crate::CacheManager;
use crate::cache_entry;
use crate::key::validate_cache_key;

/// Messages the `cache` unit answers.
///
/// One variant per operation, no namespace or key-form variants: the
/// favicon's needs are the whole contract today. Anything else is a new
/// variant by explicit change.
#[derive(Debug)]
pub enum CacheMsg {
    /// Fetch the entry stored under `key` in the core namespace.
    Get {
        /// The cache key to look up.
        key: String,
    },
    /// Store `value` under `key` in the core namespace, replacing any stale
    /// entry, with `ttl` counted from creation.
    Insert {
        /// The cache key to store under.
        key: String,
        /// The bytes to store.
        value: Vec<u8>,
        /// How long the entry lives, counted from creation.
        ttl: Duration,
    },
}

/// Replies from the `cache` unit.
///
/// Owned data only — nothing borrows from the manager. `Rejected` and
/// `Failed` are distinct from a miss on purpose: callers must be able to
/// tell "nothing cached" apart from "the cache declined to serve".
#[derive(Debug)]
pub enum CacheResp {
    /// The reply to [`CacheMsg::Get`]: the stored bytes, or `None` when the
    /// key is absent or its entry was already reclaimed.
    Value(Option<Vec<u8>>),
    /// The reply to [`CacheMsg::Insert`]: the value is stored under the key.
    Inserted,
    /// The key failed the same validation cachegirl enforces on resource
    /// names. The request never reached the backend.
    Rejected {
        /// A short static reason describing the violated rule.
        reason: &'static str,
    },
    /// The backend (cachegirl) refused or failed the operation — quota,
    /// manager closed, poisoned lock, backend error. The detail is the
    /// error rendered for operators; it may carry the key when the backend
    /// echoed it.
    Failed {
        /// What went wrong, for logs.
        detail: String,
    },
}

/// Handle one message against `manager`.
///
/// Pure with respect to the tree: no IPC happens here, the caller
/// ([`cache_spec`]) is the transport. Split out so the semantics are
/// documented next to the manager they operate on.
fn handle_msg(manager: &CacheManager, msg: CacheMsg) -> CacheResp {
    match msg {
        CacheMsg::Get { key } => {
            if let Err(reason) = validate_cache_key(&key) {
                return CacheResp::Rejected { reason };
            }
            let namespace = match manager.core_cache() {
                Ok(namespace) => namespace,
                Err(error) => {
                    return CacheResp::Failed {
                        detail: error.to_string(),
                    };
                }
            };
            match cache_entry::get(&namespace, &key) {
                Ok(Some(entry)) => CacheResp::Value(Some(entry.value)),
                Ok(None) => CacheResp::Value(None),
                Err(error) => CacheResp::Failed {
                    detail: error.to_string(),
                },
            }
        }
        CacheMsg::Insert { key, value, ttl } => {
            if let Err(reason) = validate_cache_key(&key) {
                return CacheResp::Rejected { reason };
            }
            let namespace = match manager.core_cache() {
                Ok(namespace) => namespace,
                Err(error) => {
                    return CacheResp::Failed {
                        detail: error.to_string(),
                    };
                }
            };
            // cachegirl keys are unique per namespace; the stale entry has to
            // go first or the insert is rejected as a duplicate. A remove
            // failure means the insert would hit the same wall anyway, so it
            // reports the failure instead of piling a second error on top.
            if let Err(error) = cache_entry::remove(&namespace, &key) {
                return CacheResp::Failed {
                    detail: error.to_string(),
                };
            }
            match cache_entry::insert(&namespace, &key, value, ttl) {
                Ok(_) => CacheResp::Inserted,
                Err(error) => CacheResp::Failed {
                    detail: error.to_string(),
                },
            }
        }
    }
}

/// Describe the `cache` unit for spawning.
///
/// The composition root hands the live [`CacheManager`] in; the unit owns one
/// clone and serves it until the process exits or the tree restarts it. The
/// manager itself is disposable by design (its instance directory is removed
/// on shutdown), which is what makes a restart an honest recovery: callers
/// simply see misses until something re-inserts.
///
/// The unit runs `LongLived` under an `Always` policy: losing the cache
/// front is not a recoverable state for the callers holding only the pid,
/// so the supervisor replaces the unit instead of leaving them degraded.
#[must_use]
pub fn cache_spec(manager: Arc<CacheManager>) -> UnitSpec {
    UnitSpec::service(
        "cache",
        UnitKind::LongLived,
        RestartPolicy::Always,
        move |_ctx, msg: CacheMsg| {
            let manager = Arc::clone(&manager);
            async move { handle_msg(&manager, msg) }
        },
    )
    .with_service_kind(ServiceKind::Cache)
}

/// Call the `cache` unit with `msg`.
///
/// `caller` is the unit the call is attributed to. Inside a unit body the
/// caller is the body's own unit, and the unit must hold a grant to the
/// cache unit — the grant is installed at spawn, exactly like an inherited
/// fd. Code outside any unit body attributes to [`Pid::INIT`], which is an
/// ancestor of every unit, so the subtree rule authorizes the call; the
/// boot-time warm-up and the reload hand-off use that path.
///
/// # Errors
///
/// Whatever the tree reports: `NotRunning` (the unit is down), `Busy` (its
/// mailbox is full), `Timeout`, `Panicked`, `AccessDenied` (the caller is a
/// sibling unit without the grant) or `TypeMismatch`.
pub async fn cache_call(
    init: &Init,
    caller: Pid,
    target: Pid,
    msg: CacheMsg,
) -> Result<CacheResp, CallError> {
    init.call(caller, target, msg).await
}

#[cfg(test)]
mod tests {
    // no fakes: the tests spawn the real unit over a real tree, on loopback
    // nothing and with no network, exactly the way the composition root
    // wires it
    use super::*;

    use candyriya_init::current_unit;
    use candyriya_scheduler::TaskService;

    fn spawn_test_cache() -> (Init, Pid, Arc<CacheManager>, tempfile::TempDir) {
        let service = TaskService::builder()
            .handle(tokio::runtime::Handle::current())
            .blocking_threads(1)
            .build()
            .expect("task service builds");
        let root = tempfile::tempdir().expect("tempdir");
        let manager = Arc::new(CacheManager::new(root.path()).expect("cache manager"));
        let init = Init::new(service);
        let pid = init
            .spawn_unit(Pid::INIT, cache_spec(Arc::clone(&manager)))
            .expect("cache unit spawns");
        (init, pid, manager, root)
    }

    #[tokio::test]
    async fn insert_then_get_roundtrips_the_value() {
        let (init, pid, manager, _root) = spawn_test_cache();
        let payload = b"\x89PNG\r\n\x1a\nfake-icon".to_vec();

        let reply = cache_call(
            &init,
            Pid::INIT,
            pid,
            CacheMsg::Insert {
                key: "status.favicon".to_owned(),
                value: payload.clone(),
                ttl: Duration::from_secs(60),
            },
        )
        .await
        .expect("the unit answers the insert");
        assert!(matches!(reply, CacheResp::Inserted));

        let reply = cache_call(
            &init,
            Pid::INIT,
            pid,
            CacheMsg::Get {
                key: "status.favicon".to_owned(),
            },
        )
        .await
        .expect("the unit answers the get");
        let CacheResp::Value(Some(bytes)) = reply else {
            panic!("expected the stored bytes, got {reply:?}");
        };
        assert_eq!(bytes, payload);

        manager.shutdown().expect("cache shutdown");
    }

    #[tokio::test]
    async fn get_on_an_absent_key_returns_none() {
        let (init, pid, manager, _root) = spawn_test_cache();
        let reply = cache_call(
            &init,
            Pid::INIT,
            pid,
            CacheMsg::Get {
                key: "never.stored".to_owned(),
            },
        )
        .await
        .expect("the unit answers the get");
        assert!(matches!(reply, CacheResp::Value(None)));

        manager.shutdown().expect("cache shutdown");
    }

    #[tokio::test]
    async fn an_invalid_key_is_rejected_without_touching_the_backend() {
        let (init, pid, manager, _root) = spawn_test_cache();
        for bad in [
            "a/b",
            "a:b",
            ".",
            "trailing.",
            "NUL.txt",
            " padded",
            "with\0nul",
        ] {
            let reply = cache_call(
                &init,
                Pid::INIT,
                pid,
                CacheMsg::Get {
                    key: bad.to_owned(),
                },
            )
            .await
            .expect("the unit answers, refusing the key");
            assert!(
                matches!(reply, CacheResp::Rejected { .. }),
                "{bad:?} must be rejected"
            );

            let reply = cache_call(
                &init,
                Pid::INIT,
                pid,
                CacheMsg::Insert {
                    key: bad.to_owned(),
                    value: b"x".to_vec(),
                    ttl: Duration::from_secs(60),
                },
            )
            .await
            .expect("the unit answers, refusing the key");
            assert!(
                matches!(reply, CacheResp::Rejected { .. }),
                "{bad:?} must be rejected"
            );
        }

        manager.shutdown().expect("cache shutdown");
    }

    #[tokio::test]
    async fn insert_replaces_a_stale_entry() {
        let (init, pid, manager, _root) = spawn_test_cache();
        for payload in [b"first".as_slice(), b"second".as_slice()] {
            let reply = cache_call(
                &init,
                Pid::INIT,
                pid,
                CacheMsg::Insert {
                    key: "status.favicon".to_owned(),
                    value: payload.to_vec(),
                    ttl: Duration::from_secs(60),
                },
            )
            .await
            .expect("the unit answers the insert");
            assert!(matches!(reply, CacheResp::Inserted));
        }
        let reply = cache_call(
            &init,
            Pid::INIT,
            pid,
            CacheMsg::Get {
                key: "status.favicon".to_owned(),
            },
        )
        .await
        .expect("the unit answers the get");
        let CacheResp::Value(Some(bytes)) = reply else {
            panic!("expected the stored bytes, got {reply:?}");
        };
        assert_eq!(bytes, b"second");

        manager.shutdown().expect("cache shutdown");
    }

    #[tokio::test]
    async fn a_sibling_without_a_grant_is_refused() {
        let (init, pid, manager, _root) = spawn_test_cache();
        let probe = spawn_sibling(&init, pid, false).await;
        let exit = wait_exit(&init, probe).await;
        assert!(
            !matches!(exit, candyriya_init::ExitStatus::Success),
            "a sibling without the grant must not read the cache, got {exit}"
        );

        manager.shutdown().expect("cache shutdown");
    }

    #[tokio::test]
    async fn a_cancelled_unit_refuses_the_call_with_not_running() {
        let (init, pid, _manager, _root) = spawn_test_cache();
        init.cancel(pid).expect("the unit cancels");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let state = init.unit(pid).map(|unit| unit.state);
            if !matches!(state, Some(candyriya_init::UnitState::Running)) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the cache unit did not stop within the deadline"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let err = cache_call(
            &init,
            Pid::INIT,
            pid,
            CacheMsg::Get {
                key: "status.favicon".to_owned(),
            },
        )
        .await
        .expect_err("a down unit must not serve a value");
        assert_eq!(err, CallError::NotRunning);
    }

    /// A one-shot probe unit that reads the cache inside its body, exactly
    /// where the `CURRENT_UNIT` task-local is the probe itself. Returns the
    /// probe's pid.
    async fn spawn_sibling(init: &Init, cache_pid: Pid, with_grant: bool) -> Pid {
        let init_for_body = init.clone();
        let mut spec = UnitSpec::new(
            "cache.probe",
            UnitKind::LongLived,
            RestartPolicy::Never,
            move || {
                let init = init_for_body.clone();
                async move { read_inside_body(&init, cache_pid).await }
            },
        );
        if with_grant {
            spec = spec.with_grant(cache_pid);
        }
        init.spawn_unit(Pid::INIT, spec).expect("probe spawns")
    }

    /// Read through the unit attributing the call to the calling unit itself;
    /// the outcome rides out as the probe's exit status.
    async fn read_inside_body(init: &Init, target: Pid) -> candyriya_init::ExitStatus {
        let caller = current_unit().expect("the body runs as its unit");
        match cache_call(
            init,
            caller,
            target,
            CacheMsg::Get {
                key: "status.favicon".to_owned(),
            },
        )
        .await
        {
            Ok(_) => candyriya_init::ExitStatus::Success,
            Err(error) => candyriya_init::ExitStatus::failure(format!("call failed: {error}")),
        }
    }

    /// Wait until the probe exits and hand back its exit status; the
    /// reaper's own records are the source of truth.
    async fn wait_exit(init: &Init, pid: Pid) -> candyriya_init::ExitStatus {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            for outcome in init.reap_children(Pid::INIT) {
                match outcome {
                    candyriya_init::ReapOutcome::Reaped(record)
                    | candyriya_init::ReapOutcome::Restarting { record, .. }
                    | candyriya_init::ReapOutcome::FailedTerminal { record } => {
                        if record.pid == pid {
                            return record.status;
                        }
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the probe did not finish within the deadline"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}
