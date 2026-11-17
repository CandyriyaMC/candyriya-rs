//! The `config` unit: the supervised holder of the live configuration.
//!
//! Every runtime read of the proxy configuration goes through this unit as a
//! typed IPC call (see the module docs of [`candyriya_init`]); no subsystem
//! reads the config store directly once the tree is up. The composition root
//! spawns the unit at boot with the live [`ConfigStore`], and answers
//! [`ConfigMsg::Get`] with a cheap snapshot of it.
//!
//! # Why the protocol lives here
//!
//! The message pair is the contract between the composition root (which
//! spawns the unit and attaches the handler) and the proxy crate (whose call
//! sites consume it). It is deliberately small: one request enum, one
//! response enum, zero serialization — the tree type-erases envelopes and
//! downcasts on the callee. This is internal wiring, not a stabilized public
//! API; adding a variant is an explicit, reviewable change.
//!
//! # Reply cost
//!
//! [`ConfigStore::get`] is an `ArcSwap` load: an atomic read plus a refcount
//! bump. The reply therefore carries the same [`Arc<Config>`] the store holds
//! — zero-copy, and the snapshot semantics are preserved (a reader keeps the
//! exact version it started with even if a reload swaps in a new one, which
//! is what a `/candyriya reload` on the other end of the process needs).
//!
//! # Capability
//!
//! Units call [`Init::call`] with their own pid; the subtree rule alone
//! does not cover sibling calls, so a unit that reads the configuration
//! needs a grant to this unit, installed at its spawn (the composition root
//! grants one to `proxy.accept`). Code outside unit bodies attributes to
//! the root, which is an ancestor of everything.
//!
//! # Failure vocabulary
//!
//! Callers see the tree's [`CallError`](candyriya_init::CallError). A reply
//! is only ever produced by a running unit; a missing or crashed unit fails
//! fast with `NotRunning`, a live-but-slow one with `Timeout`. The unit is
//! spawned with [`RestartPolicy::Always`] — it is the source of truth for
//! the configuration, so a restart is the correct recovery — but the
//! supervision window in between can refuse a call; every call site decides
//! for itself how to fail (see the callers for their policy).

use std::sync::Arc;

use candyriya_config::{Config, ConfigStore};
use candyriya_init::{Init, Pid, RestartPolicy, ServiceKind, UnitKind, UnitSpec};

/// Messages the `config` unit answers.
///
/// One variant on purpose: reads are the only operation today. A reload path
/// over IPC is a future, additive change, not something callers may assume
/// exists.
#[derive(Debug)]
pub enum ConfigMsg {
    /// Fetch a point-in-time snapshot of the live configuration.
    Get,
}

/// Replies from the `config` unit.
#[derive(Debug)]
pub enum ConfigResp {
    /// The snapshot at the time the handler ran. Cheap to clone; the caller
    /// observes exactly this version.
    Snapshot(Arc<Config>),
}

/// Describe the `config` unit for spawning.
///
/// The composition root (or a test) moves an [`Arc<ConfigStore>`] in; the
/// unit owns one clone and serves it until the process exits or the tree
/// restarts it. The handler performs no I/O beyond the snapshot load, so the
/// mailbox stays cold even at ping/login rates.
///
/// The unit runs `LongLived` under an `Always` policy: losing the config is
/// not a recoverable state for the rest of the tree, so the supervisor
/// replaces the unit instead of leaving callers to degrade.
#[must_use]
pub fn config_spec(store: Arc<ConfigStore>) -> UnitSpec {
    UnitSpec::service(
        "config",
        UnitKind::LongLived,
        RestartPolicy::Always,
        move |_ctx, msg: ConfigMsg| {
            let store = Arc::clone(&store);
            async move {
                match msg {
                    ConfigMsg::Get => ConfigResp::Snapshot(store.get()),
                }
            }
        },
    )
    .with_service_kind(ServiceKind::Config)
}

/// Fetch the current configuration snapshot through the tree.
///
/// `caller` is the unit the call is attributed to. Inside a unit body the
/// caller is the body's own unit, and the unit must hold a grant to the
/// config unit — the grant is installed at spawn, exactly like an inherited
/// fd (the composition root grants one to `proxy.accept`). Code outside any
/// unit body attributes to [`Pid::INIT`], which is an ancestor of every
/// unit, so the subtree rule authorizes the call; per-connection tasks use
/// that path today.
///
/// # Errors
///
/// Whatever the tree reports: `NotRunning` (the unit is down), `Busy` (its
/// mailbox is full), `Timeout`, `Panicked`, `AccessDenied` (the caller is a
/// sibling unit without the grant) or `TypeMismatch`.
pub async fn config(
    init: &Init,
    caller: Pid,
    target: Pid,
) -> Result<Arc<Config>, candyriya_init::CallError> {
    match init.call(caller, target, ConfigMsg::Get).await? {
        ConfigResp::Snapshot(snapshot) => Ok(snapshot),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // no fakes: the tests spawn the real unit over a real tree, on loopback
    // nothing and with no network, exactly the way the composition root
    // wires it
    async fn spawned(config: Config) -> (Init, Pid, Arc<ConfigStore>) {
        let service = candyriya_scheduler::TaskService::builder()
            .handle(tokio::runtime::Handle::current())
            .blocking_threads(1)
            .build()
            .expect("task service builds");
        let store = Arc::new(ConfigStore::new(config));
        let init = Init::new(service);
        let pid = init
            .spawn_unit(Pid::INIT, config_spec(Arc::clone(&store)))
            .expect("config unit spawns");
        (init, pid, store)
    }

    fn toml(motd: &str) -> String {
        format!("config-version = \"0.1\"\nmotd = \"{motd}\"\n")
    }

    #[tokio::test]
    async fn the_unit_answers_get_with_the_snapshot_it_holds() {
        let (init, pid, store) = spawned(Config::default()).await;
        let reply = init
            .call::<ConfigMsg, ConfigResp>(Pid::INIT, pid, ConfigMsg::Get)
            .await
            .expect("the unit answers");
        let ConfigResp::Snapshot(snapshot) = reply;
        // same allocation the store holds: a snapshot, not a rebuild
        assert!(Arc::ptr_eq(&snapshot, &store.get()));
    }

    #[tokio::test]
    async fn a_second_get_after_a_reload_sees_the_new_value() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("candyriya.toml");
        std::fs::write(&path, toml("<aqua>one")).expect("write v1");
        let store = ConfigStore::from_path(&path).expect("load v1");
        let store = Arc::new(store);

        let service = candyriya_scheduler::TaskService::builder()
            .handle(tokio::runtime::Handle::current())
            .blocking_threads(1)
            .build()
            .expect("task service builds");
        let init = Init::new(service);
        let pid = init
            .spawn_unit(Pid::INIT, config_spec(Arc::clone(&store)))
            .expect("config unit spawns");

        let first = config(&init, Pid::INIT, pid).await.expect("first get");
        assert_eq!(first.motd, "<aqua>one");

        std::fs::write(&path, toml("<aqua>two")).expect("write v2");
        store.reload(&path).expect("reload");

        let second = config(&init, Pid::INIT, pid).await.expect("second get");
        assert_eq!(second.motd, "<aqua>two");
    }

    #[tokio::test]
    async fn a_sibling_unit_with_a_grant_can_read_the_config() {
        let (init, pid, _store) = spawned(Config::default()).await;
        let probe = spawn_sibling(&init, pid, true).await;
        let exit = wait_exit(&init, probe).await;
        assert!(
            matches!(exit, candyriya_init::ExitStatus::Success),
            "a sibling holding the grant reads the snapshot, got {exit}"
        );
    }

    #[tokio::test]
    async fn a_sibling_unit_without_a_grant_is_refused() {
        let (init, pid, _store) = spawned(Config::default()).await;
        let probe = spawn_sibling(&init, pid, false).await;
        let exit = wait_exit(&init, probe).await;
        assert!(
            !matches!(exit, candyriya_init::ExitStatus::Success),
            "a sibling without the grant must not read the snapshot, got {exit}"
        );
    }

    /// A one-shot probe unit that reads the config inside its body, exactly
    /// where the `CURRENT_UNIT` task-local is the probe itself. Returns the
    /// probe's pid.
    async fn spawn_sibling(init: &Init, config_pid: Pid, with_grant: bool) -> Pid {
        let init_for_body = init.clone();
        let mut spec = UnitSpec::new(
            "config.probe",
            UnitKind::LongLived,
            RestartPolicy::Never,
            move || {
                let init = init_for_body.clone();
                async move { read_inside_body(&init, config_pid).await }
            },
        );
        if with_grant {
            spec = spec.with_grant(config_pid);
        }
        init.spawn_unit(Pid::INIT, spec).expect("probe spawns")
    }

    /// Read the snapshot attributing the call to the calling unit itself;
    /// the outcome rides out as the probe's exit status.
    async fn read_inside_body(init: &Init, target: Pid) -> candyriya_init::ExitStatus {
        let caller = candyriya_init::current_unit().expect("the body runs as its unit");
        match config(init, caller, target).await {
            Ok(_) => candyriya_init::ExitStatus::Success,
            Err(error) => candyriya_init::ExitStatus::failure(format!("call failed: {error}")),
        }
    }

    /// Wait until the probe exits and hand back its exit status; the
    /// reaper's own records are the source of truth.
    async fn wait_exit(init: &Init, pid: Pid) -> candyriya_init::ExitStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
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
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn a_cancelled_unit_refuses_the_call_with_not_running() {
        let (init, pid, _store) = spawned(Config::default()).await;
        init.cancel(pid).expect("the unit cancels");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let state = init.unit(pid).map(|unit| unit.state);
            if !matches!(state, Some(candyriya_init::UnitState::Running)) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the config unit did not stop within the deadline"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let err = config(&init, Pid::INIT, pid)
            .await
            .expect_err("a down unit must not serve a snapshot");
        assert_eq!(err, candyriya_init::CallError::NotRunning);
    }
}
