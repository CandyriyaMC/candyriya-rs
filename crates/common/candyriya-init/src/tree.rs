//! The supervision tree itself: registry, zombies, reaping and restarts.
//!
//! [`Init`] is the root handle of the tree. It owns pid allocation, the
//! node registry, the bounded ring of recent exits and all the counters
//! that back the `candyriya_zombies*` / `candyriya_services_*` metrics.
//! Cloning it is cheap; every clone talks to the same underlying state.
//!
//! Concurrency model: a single `parking_lot::Mutex` guards the whole
//! registry. Critical sections are short (map updates, counter bumps,
//! small vector allocations) and never contain an `.await`, so contention
//! is negligible and lock ordering against the task service is always
//! tree → service.
//!
//! Bootstrap units are not implemented here: this module is the tree
//! mechanics only. The root unit (pid 1) is created eagerly and never
//! runs a body of its own; its lifecycle is the process's lifecycle.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use candyriya_scheduler::{TaskId, TaskService};

use crate::error::InitError;
use crate::exit::{ExitRecord, ExitStatus};
use crate::node::{UnitInfo, UnitKind, UnitState};
use crate::pid::Pid;
use crate::policy::{AttemptWindow, PolicyDecision, RestartLimits, RestartPolicy, evaluate};
use crate::ring::RecentExits;
use crate::spec::{UnitBodyFactory, UnitSpec};

/// Upper bound of a unit name, in bytes. Names go into logs, metrics
/// labels and the process table; 128 bytes is generous and keeps labels sane.
pub const MAX_UNIT_NAME_BYTES: usize = 128;

/// Knobs for the tree's behaviour.
///
/// `Default` is the production configuration (exit ring of 64, 30s stuck
/// threshold, the documented restart limits). Tests tighten the knobs to
/// keep them fast.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InitOptions {
    /// How many recent exits the ring remembers. At least 1.
    pub exit_ring_capacity: usize,
    /// How long a zombie may sit unreaped before it counts as stuck.
    pub stuck_threshold: Duration,
    /// Backoff and start-limit parameters for restarts.
    pub restart_limits: RestartLimits,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            exit_ring_capacity: 64,
            stuck_threshold: Duration::from_secs(30),
            restart_limits: RestartLimits::default(),
        }
    }
}

/// Point-in-time counters for the tree, for ps/metrics/tests.
///
/// Counters are cumulative since the tree was built; the `_current`-style
/// fields are instantaneous.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeSnapshot {
    /// Units currently in the registry, including the root and terminal units.
    pub units: usize,
    /// Units in the `Running` state.
    pub running: usize,
    /// Units in the `Zombie` state at snapshot time.
    pub zombies: usize,
    /// Units in the `Restarting` state at snapshot time (waiting out a backoff).
    pub restarting: usize,
    /// Units in the `FailedTerminal` state.
    pub failed_terminal: usize,
    /// Zombies older than the stuck threshold at snapshot time.
    pub stuck_zombies: usize,
    /// Times any unit has been reaped.
    pub reaps_total: u64,
    /// Units whose body has been re-created after a restart decision.
    pub restarts_total: u64,
    /// Units moved to `FailedTerminal` because their start limit was breached.
    pub failed_terminal_total: u64,
    /// Exits observed and pushed into the ring (cumulative).
    pub exits_recorded_total: u64,
    /// Records currently held in the exit ring.
    pub exit_ring_len: usize,
    /// Capacity of the exit ring.
    pub exit_ring_capacity: usize,
    /// Records evicted from the exit ring because it was full.
    pub exit_ring_dropped: u64,
}

/// What became of a zombie at reap time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReapOutcome {
    /// The zombie was harvested and removed from the registry. The exit
    /// record is also in the ring.
    Reaped(ExitRecord),
    /// A restart was scheduled: the unit is now in `Restarting` and its
    /// body will be re-created after `after` (plus the stop-requested
    /// check at fire time).
    Restarting {
        /// The exit that triggered the restart.
        record: ExitRecord,
        /// How long the tree will wait before re-creating the body.
        after: Duration,
    },
    /// The start limit was breached. The unit moved to `FailedTerminal`
    /// and stays visible in the registry; it never restarts.
    FailedTerminal {
        /// The exit that breached the limit.
        record: ExitRecord,
    },
}

/// Registry entry for a unit. Crate-private: the public read model is
/// [`UnitInfo`].
pub(crate) struct UnitNode {
    pub pid: Pid,
    pub ppid: Pid,
    pub name: String,
    pub kind: UnitKind,
    pub state: UnitState,
    pub started_at: Instant,
    pub restart_count: u32,
    pub last_exit: Option<ExitStatus>,
    pub exited_at: Option<Instant>,
    policy: RestartPolicy,
    factory: UnitBodyFactory,
    timeout: Option<Duration>,
    attempts: AttemptWindow,
    stop_requested: bool,
    service_id: Option<TaskId>,
}

impl UnitNode {
    fn info(&self) -> UnitInfo {
        UnitInfo {
            pid: self.pid,
            ppid: self.ppid,
            name: self.name.clone(),
            kind: self.kind,
            state: self.state,
            started_at: self.started_at,
            restart_count: self.restart_count,
            last_exit: self.last_exit.clone(),
            exited_at: self.exited_at,
        }
    }
}

/// All mutable tree state, behind one mutex.
pub(crate) struct TreeState {
    nodes: HashMap<Pid, UnitNode>,
    next_raw_pid: u64,
    exits: RecentExits,
    zombies_current: u64,
    reaps_total: u64,
    restarts_total: u64,
    failed_terminal_total: u64,
    options: InitOptions,
}

impl TreeState {
    fn new(options: InitOptions) -> Self {
        let root = UnitNode {
            pid: Pid::INIT,
            ppid: Pid::NONE,
            name: "init".to_string(),
            kind: UnitKind::LongLived,
            state: UnitState::Running,
            started_at: Instant::now(),
            restart_count: 0,
            last_exit: None,
            exited_at: None,
            policy: RestartPolicy::Never,
            factory: Arc::new(|| Box::pin(std::future::pending::<ExitStatus>())),
            timeout: None,
            attempts: AttemptWindow::new(),
            stop_requested: false,
            service_id: None,
        };
        let mut nodes = HashMap::with_capacity(16);
        nodes.insert(Pid::INIT, root);
        Self {
            nodes,
            next_raw_pid: 2,
            exits: RecentExits::new(options.exit_ring_capacity),
            zombies_current: 0,
            reaps_total: 0,
            restarts_total: 0,
            failed_terminal_total: 0,
            options,
        }
    }

    fn stuck_count(&self, now: Instant) -> usize {
        self.nodes
            .values()
            .filter(|node| node.state == UnitState::Zombie)
            .filter(|node| {
                node.exited_at.is_some_and(|at| {
                    now.saturating_duration_since(at) > self.options.stuck_threshold
                })
            })
            .count()
    }

    fn emit_zombie_metrics(&self) {
        metrics::gauge!("candyriya_zombies").set(self.zombies_current as f64);
        metrics::gauge!("candyriya_zombies_stuck").set(self.stuck_count(Instant::now()) as f64);
    }

    /// Record an exit reported by a unit wrapper.
    ///
    /// The unit must be `Running`; anything else is a duplicate or a
    /// late-arriving record and is dropped with a debug log. Orphans are
    /// re-parented to the root here — at exit time, so they are never
    /// addressable to a dead parent even briefly.
    pub(crate) fn record_exit(&mut self, pid: Pid, status: ExitStatus) {
        let now = Instant::now();
        let Some(node) = self.nodes.get_mut(&pid) else {
            tracing::debug!(%pid, "exit for a unit that is already gone; dropping it");
            return;
        };
        if node.state != UnitState::Running {
            tracing::debug!(%pid, state = %node.state, "duplicate exit for unit; keeping the first");
            return;
        }
        node.state = UnitState::Zombie;
        node.exited_at = Some(now);
        node.last_exit = Some(status.clone());
        self.zombies_current += 1;
        let record = ExitRecord {
            pid,
            ppid: node.ppid,
            name: node.name.clone(),
            kind: node.kind,
            status: status.clone(),
            restart_count: node.restart_count,
            started_at: node.started_at,
            exited_at: now,
        };
        self.exits.push(record);

        // orphans re-parent to the root; the ring keeps the ppid at exit time
        let orphans: Vec<Pid> = self
            .nodes
            .values()
            .filter(|child| child.pid != pid && child.ppid == pid)
            .map(|child| child.pid)
            .collect();
        for orphan in orphans {
            if let Some(child) = self.nodes.get_mut(&orphan) {
                child.ppid = Pid::INIT;
            }
        }
        self.emit_zombie_metrics();
    }

    /// Harvest one zombie, evaluating its restart policy inline.
    ///
    /// Synchronous on purpose: the reaper is the only place a restart
    /// decision is made, so there is exactly one code path that can move a
    /// unit between `Zombie`, `Restarting`, `FailedTerminal` and removed.
    fn reap_locked(&mut self, pid: Pid, now: Instant) -> Result<ReapOutcome, InitError> {
        let node = self.nodes.get_mut(&pid).ok_or(InitError::NoSuchUnit(pid))?;
        if node.state != UnitState::Zombie {
            return Err(InitError::NotZombie {
                pid,
                state: node.state,
            });
        }
        let exited_at = node
            .exited_at
            .take()
            .ok_or(InitError::MissingExitStatus(pid))?;
        let status = node
            .last_exit
            .take()
            .ok_or(InitError::MissingExitStatus(pid))?;
        let record = ExitRecord {
            pid,
            ppid: node.ppid,
            name: node.name.clone(),
            kind: node.kind,
            status: status.clone(),
            restart_count: node.restart_count,
            started_at: node.started_at,
            exited_at,
        };

        self.reaps_total += 1;
        self.zombies_current = self.zombies_current.saturating_sub(1);
        metrics::counter!("candyriya_zombie_reaps_total").increment(1);

        let recent_attempts = node
            .attempts
            .len_at(now, self.options.restart_limits.window);
        match evaluate(
            node.policy,
            &status,
            node.stop_requested,
            node.restart_count,
            recent_attempts,
            &self.options.restart_limits,
        ) {
            PolicyDecision::Remove => {
                self.nodes.remove(&pid);
                self.emit_zombie_metrics();
                Ok(ReapOutcome::Reaped(record))
            }
            PolicyDecision::FailedTerminal => {
                node.state = UnitState::FailedTerminal;
                node.last_exit = Some(status);
                node.exited_at = Some(exited_at);
                self.failed_terminal_total += 1;
                metrics::counter!("candyriya_services_failed_terminal_total").increment(1);
                self.emit_zombie_metrics();
                Ok(ReapOutcome::FailedTerminal { record })
            }
            PolicyDecision::ScheduleRestart { after } => {
                node.state = UnitState::Restarting;
                node.last_exit = Some(status);
                node.exited_at = Some(exited_at);
                self.emit_zombie_metrics();
                Ok(ReapOutcome::Restarting { record, after })
            }
        }
    }

    /// Re-create a unit whose backoff has expired.
    ///
    /// Called from the delayed task scheduled by [`Init::reap`]; it folds a
    /// stop that was requested during the backoff into a plain removal, and
    /// re-checks the `Restarting` state because other paths can finalize
    /// the node in the meantime (a cancel, or a second scheduling bug).
    fn respawn_locked(
        &mut self,
        pid: Pid,
        tree: &Arc<Mutex<TreeState>>,
        service: &TaskService,
    ) -> bool {
        let now = Instant::now();
        let decision = match self.nodes.get_mut(&pid) {
            None => None,
            Some(node) => {
                if node.state != UnitState::Restarting {
                    None
                } else if node.stop_requested {
                    self.nodes.remove(&pid);
                    tracing::info!(%pid, "stop requested during backoff; unit not restarted");
                    return false;
                } else {
                    node.attempts
                        .record(now, self.options.restart_limits.window);
                    node.restart_count = node.restart_count.saturating_add(1);
                    node.started_at = now;
                    node.exited_at = None;
                    node.state = UnitState::Running;
                    Some((node.name.clone(), node.factory.clone(), node.timeout))
                }
            }
        };
        let Some((name, factory, timeout)) = decision else {
            return false;
        };
        self.restarts_total += 1;
        metrics::counter!("candyriya_services_restarts_total").increment(1);
        let service_id =
            crate::runner::spawn_wrapper(service, Arc::clone(tree), pid, &name, factory, timeout);
        if let Some(node) = self.nodes.get_mut(&pid) {
            node.service_id = Some(service_id);
        }
        true
    }
}

/// The root handle of the supervision tree.
///
/// Cheap to clone; all clones share the same underlying state. The root
/// unit (pid 1, named `init`) is created eagerly with [`Pid::NONE`] as
/// parent and never runs a body of its own.
///
/// # Example
///
/// ```no_run
/// use candyriya_init::{Init, Pid, RestartPolicy, UnitKind, UnitSpec};
/// use candyriya_scheduler::TaskService;
/// use std::time::Duration;
///
/// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).build()?;
/// let service = TaskService::builder().handle(rt.handle().clone()).blocking_threads(2).build()?;
/// let tree = Init::new(service);
/// assert_eq!(tree.root_pid(), Pid::INIT);
///
/// let pid = tree.spawn_unit(
///     Pid::INIT,
///     UnitSpec::new("heartbeat", UnitKind::Every, RestartPolicy::Never, || async {
///         candyriya_init::ExitStatus::Success
///     }),
/// )?;
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Init {
    inner: Arc<Mutex<TreeState>>,
    service: TaskService,
}

impl Init {
    /// Create the tree with the default [`InitOptions`].
    pub fn new(service: TaskService) -> Self {
        Self::with_options(service, InitOptions::default())
    }

    /// Create the tree with explicit options.
    pub fn with_options(service: TaskService, options: InitOptions) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TreeState::new(options))),
            service,
        }
    }

    /// The pid of the root supervisor, always [`Pid::INIT`].
    #[must_use]
    pub fn root_pid(&self) -> Pid {
        Pid::INIT
    }

    /// The task service backing the tree.
    #[must_use]
    pub fn service(&self) -> &TaskService {
        &self.service
    }

    /// The options the tree was built with.
    #[must_use]
    pub fn options(&self) -> InitOptions {
        self.inner.lock().options
    }

    /// Spawn a unit as a child of `parent`.
    ///
    /// The parent must exist and be `Running` — a zombie cannot supervise
    /// anything, and pid reuse does not exist. The body is created
    /// immediately and runs through the central task service; the tree only
    /// tracks its lifecycle.
    ///
    /// # Errors
    ///
    /// - [`InitError::NoSuchParent`] — no unit with pid `parent` exists.
    /// - [`InitError::ParentNotRunning`] — the parent is not (or no longer) running.
    /// - [`InitError::InvalidName`] — the name is empty, over 128 bytes or
    ///   contains control characters.
    pub fn spawn_unit(&self, parent: Pid, spec: UnitSpec) -> Result<Pid, InitError> {
        validate_name(&spec.name)?;
        let mut st = self.inner.lock();
        let parent_node = st
            .nodes
            .get(&parent)
            .ok_or(InitError::NoSuchParent(parent))?;
        if parent_node.state != UnitState::Running {
            return Err(InitError::ParentNotRunning(parent));
        }
        let pid = Pid::new(st.next_raw_pid);
        st.next_raw_pid += 1;
        // The tree lock is held across the spawn: the wrapper can never run
        // before the node exists (a fast exit would otherwise race the
        // insert and lose its status), and a concurrent cancel() blocks
        // until the unit is fully registered.
        let service_id = crate::runner::spawn_wrapper(
            &self.service,
            Arc::clone(&self.inner),
            pid,
            &spec.name,
            spec.factory.clone(),
            spec.timeout,
        );
        st.nodes.insert(
            pid,
            UnitNode {
                pid,
                ppid: parent,
                name: spec.name,
                kind: spec.kind,
                state: UnitState::Running,
                started_at: Instant::now(),
                restart_count: 0,
                last_exit: None,
                exited_at: None,
                policy: spec.policy,
                factory: spec.factory,
                timeout: spec.timeout,
                attempts: AttemptWindow::new(),
                stop_requested: false,
                service_id: Some(service_id),
            },
        );
        Ok(pid)
    }

    /// Request that a unit stop.
    ///
    /// Marks the unit's restart policy as suppressed, then flips the
    /// underlying cancellation token. Returns whether anything was actually
    /// cancelled: a unit that already finished (or the root, whose
    /// lifecycle is the process's) reports `false` — the flag still lands,
    /// so a stop requested during a backoff folds into a plain reap.
    ///
    /// # Errors
    ///
    /// [`InitError::NoSuchUnit`] — no unit with pid `pid` exists.
    pub fn cancel(&self, pid: Pid) -> Result<bool, InitError> {
        let mut st = self.inner.lock();
        let node = st.nodes.get_mut(&pid).ok_or(InitError::NoSuchUnit(pid))?;
        node.stop_requested = true;
        match node.service_id {
            // tree lock before service lock: always this order
            Some(id) => Ok(self.service.cancel(id)),
            None => Ok(false),
        }
    }

    /// Reap one zombie, evaluating its restart policy inline.
    ///
    /// # Errors
    ///
    /// - [`InitError::NoSuchUnit`] — no unit with pid `pid` exists.
    /// - [`InitError::NotZombie`] — the unit is not in the `Zombie` state.
    /// - [`InitError::MissingExitStatus`] — internal invariant guard.
    pub fn reap(&self, pid: Pid) -> Result<ReapOutcome, InitError> {
        let outcome = self.inner.lock().reap_locked(pid, Instant::now())?;
        self.maybe_schedule_restart(pid, &outcome);
        Ok(outcome)
    }

    /// Reap every zombie that is a direct child of `parent`.
    ///
    /// Deterministic (ascending pid order). A child whose invariant fails
    /// to reap is left in place and logged rather than silently dropped;
    /// the rest are reaped.
    pub fn reap_children(&self, parent: Pid) -> Vec<ReapOutcome> {
        let outcomes = {
            let mut st = self.inner.lock();
            let mut zombies: Vec<Pid> = st
                .nodes
                .values()
                .filter(|node| node.ppid == parent && node.state == UnitState::Zombie)
                .map(|node| node.pid)
                .collect();
            zombies.sort();
            let mut outcomes = Vec::with_capacity(zombies.len());
            for pid in zombies {
                match st.reap_locked(pid, Instant::now()) {
                    Ok(outcome) => outcomes.push(outcome),
                    Err(err) => {
                        tracing::error!(%pid, %err, "failed to reap child; leaving it a zombie")
                    }
                }
            }
            outcomes
        };
        for outcome in &outcomes {
            let pid = match outcome {
                ReapOutcome::Reaped(record)
                | ReapOutcome::Restarting { record, .. }
                | ReapOutcome::FailedTerminal { record } => record.pid,
            };
            self.maybe_schedule_restart(pid, outcome);
        }
        outcomes
    }

    /// Current number of zombies in the registry.
    #[must_use]
    pub fn zombie_count(&self) -> u64 {
        self.inner.lock().zombies_current
    }

    /// Zombies whose age exceeds the configured stuck threshold.
    #[must_use]
    pub fn stuck_zombies(&self) -> Vec<UnitInfo> {
        let now = Instant::now();
        let st = self.inner.lock();
        st.nodes
            .values()
            .filter(|node| node.state == UnitState::Zombie)
            .filter(|node| {
                node.exited_at.is_some_and(|at| {
                    now.saturating_duration_since(at) > st.options.stuck_threshold
                })
            })
            .map(UnitNode::info)
            .collect()
    }

    /// Snapshot of a single unit, if it exists.
    #[must_use]
    pub fn unit(&self, pid: Pid) -> Option<UnitInfo> {
        self.inner.lock().nodes.get(&pid).map(UnitNode::info)
    }

    /// Snapshot of every unit in the registry, ascending pid order.
    #[must_use]
    pub fn units(&self) -> Vec<UnitInfo> {
        let st = self.inner.lock();
        let mut units: Vec<UnitInfo> = st.nodes.values().map(UnitNode::info).collect();
        units.sort_by_key(|info| info.pid);
        units
    }

    /// The `limit` most recent exits, oldest first.
    ///
    /// The result is bounded by the configured ring capacity; `limit == 0`
    /// yields an empty vec.
    #[must_use]
    pub fn recent_exits(&self, limit: usize) -> Vec<ExitRecord> {
        self.inner.lock().exits.recent(limit)
    }

    /// Point-in-time counters for ps/metrics/tests.
    #[must_use]
    pub fn snapshot(&self) -> TreeSnapshot {
        let st = self.inner.lock();
        let mut snapshot = TreeSnapshot {
            units: st.nodes.len(),
            running: 0,
            zombies: 0,
            restarting: 0,
            failed_terminal: 0,
            stuck_zombies: st.stuck_count(Instant::now()),
            reaps_total: st.reaps_total,
            restarts_total: st.restarts_total,
            failed_terminal_total: st.failed_terminal_total,
            exits_recorded_total: st.exits.total_recorded(),
            exit_ring_len: st.exits.len(),
            exit_ring_capacity: st.exits.capacity(),
            exit_ring_dropped: st
                .exits
                .total_recorded()
                .saturating_sub(st.exits.len() as u64),
        };
        for node in st.nodes.values() {
            match node.state {
                UnitState::Running => snapshot.running += 1,
                UnitState::Zombie => snapshot.zombies += 1,
                UnitState::Restarting => snapshot.restarting += 1,
                UnitState::FailedTerminal => snapshot.failed_terminal += 1,
            }
        }
        snapshot
    }

    /// Schedule the delayed body re-creation for a `Restarting` outcome.
    ///
    /// The delayed task is machinery, not a unit: it never registers in the
    /// tree. At fire time it folds a stop that was requested during the
    /// backoff into a plain removal instead of restarting.
    fn maybe_schedule_restart(&self, pid: Pid, outcome: &ReapOutcome) {
        let ReapOutcome::Restarting { after, .. } = outcome else {
            return;
        };
        let tree = Arc::clone(&self.inner);
        let service = self.service.clone();
        self.service
            .delay(format!("restart:{pid}"), *after, async move {
                let mut st = tree.lock();
                st.respawn_locked(pid, &tree, &service);
            });
    }
}

fn validate_name(name: &str) -> Result<(), InitError> {
    if name.is_empty() {
        return Err(InitError::InvalidName("must not be empty".to_string()));
    }
    if name.len() > MAX_UNIT_NAME_BYTES {
        return Err(InitError::InvalidName(format!(
            "exceeds the {MAX_UNIT_NAME_BYTES}-byte limit"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(InitError::InvalidName(
            "must not contain control characters".to_string(),
        ));
    }
    Ok(())
}
