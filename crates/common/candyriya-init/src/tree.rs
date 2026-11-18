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
//! tree → service. Grant rows (`holder → targets`, mirrored for pruning)
//! live in the same state behind the same lock.
//!
//! Bootstrap units are adopted, not spawned: the loader phase runs before
//! any `Init` exists, so those steps cannot be born as units. They are
//! adopted via [`Init::adopt_unit`] once the tree is up — born as zombies
//! carrying their already-decided outcome and harvested by the same
//! reaping path as everything else. The root unit (pid 1) is created
//! eagerly and never runs a body of its own; its lifecycle is the
//! process's lifecycle.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};

use candyriya_scheduler::{TaskId, TaskService};

use crate::error::InitError;
use crate::exit::{ExitRecord, ExitStatus};
use crate::ipc::{DEFAULT_MAILBOX_CAPACITY, Envelope, Grants, NodeCallView, ServiceWiring};
use crate::node::{ServiceKind, UnitInfo, UnitKind, UnitState};
use crate::pid::Pid;
use crate::policy::{AttemptWindow, PolicyDecision, RestartLimits, RestartPolicy, evaluate};
use crate::ring::RecentExits;
use crate::spec::{UnitBody, UnitBodyFactory, UnitSpec};
use crate::stats::ConnectionInfo;

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
    // cooperative park gate: the sender side, kept for the unit's whole
    // lifetime so freeze/unfreeze can flip it at any time; each body run
    // subscribes its own receiver at spawn/respawn
    park_tx: watch::Sender<bool>,
    parked: bool,
    // per-connection counters; only connection-kind units carry them (a
    // non-connection unit pays exactly one `None`)
    stats: Option<crate::stats::ConnStats>,
    policy: RestartPolicy,
    factory: UnitBodyFactory,
    timeout: Option<Duration>,
    attempts: AttemptWindow,
    stop_requested: bool,
    service_id: Option<TaskId>,
    // ipc plane: every node owns a bounded mailbox; the receiver is held by
    // the service loop (service units) or dropped at build time (plain
    // units, whose sends fail fast with Closed)
    mailbox: Option<mpsc::Sender<Envelope>>,
    mailbox_capacity: usize,
    service_kind: Option<ServiceKind>,
    call_timeout: Option<Duration>,
    service: Option<Arc<dyn ServiceWiring>>,
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
            parked: self.parked,
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
    grants: Grants,
}

impl TreeState {
    fn new(options: InitOptions) -> Self {
        // the root owns a mailbox like everyone else; nobody services it
        let (mailbox, dead_rx) = mpsc::channel(DEFAULT_MAILBOX_CAPACITY);
        drop(dead_rx);
        let (root_park_tx, root_park_rx) = watch::channel(false);
        drop(root_park_rx);
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
            mailbox: Some(mailbox),
            mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
            service_kind: None,
            call_timeout: None,
            service: None,
            park_tx: root_park_tx,
            parked: false,
            stats: None,
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
            grants: Grants::default(),
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

    /// Resolve a node's call-relevant state into an owned view (or `None`).
    ///
    /// The caller clones the mailbox sender, so the returned view can
    /// outlive the critical section — the lock is never held across an
    /// await or a send.
    pub(crate) fn get_node(&self, pid: Pid) -> Option<NodeCallView> {
        self.nodes.get(&pid).map(|node| NodeCallView {
            state: node.state,
            mailbox: node.mailbox.clone(),
            service_kind: node.service_kind,
            call_timeout: node.call_timeout,
        })
    }

    /// Whether `ancestor` is on the ppid chain above `of`.
    ///
    /// Self does not count (the caller checks self-calls separately); the
    /// walk stops at the root (`Pid::NONE`) or at a missing link.
    pub(crate) fn is_ancestor(&self, ancestor: Pid, of: Pid) -> bool {
        let mut current = of;
        loop {
            let Some(node) = self.nodes.get(&current) else {
                return false;
            };
            if node.ppid == Pid::NONE {
                return false;
            }
            if node.ppid == ancestor {
                return true;
            }
            current = node.ppid;
        }
    }

    /// Whether `holder` currently holds a grant to `target`.
    pub(crate) fn has_grant(&self, holder: Pid, target: Pid) -> bool {
        self.grants.contains(holder, target)
    }

    /// Install a grant row, both directions.
    pub(crate) fn insert_grant(&mut self, holder: Pid, target: Pid) {
        self.grants.insert(holder, target);
    }

    /// The pids `holder` holds grants to, sorted ascending.
    pub(crate) fn grants_of(&self, holder: Pid) -> Vec<Pid> {
        self.grants.grants_of(holder)
    }

    /// Drop every grant involving `pid`, both directions.
    pub(crate) fn prune_grants(&mut self, pid: Pid) {
        self.grants.prune_involving(pid);
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
                // node is gone for good: its grants (both directions) die here
                self.prune_grants(pid);
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
                    self.prune_grants(pid);
                    tracing::info!(%pid, "stop requested during backoff; unit not restarted");
                    return false;
                } else {
                    node.attempts
                        .record(now, self.options.restart_limits.window);
                    node.restart_count = node.restart_count.saturating_add(1);
                    node.started_at = now;
                    node.exited_at = None;
                    node.state = UnitState::Running;
                    Some((
                        node.name.clone(),
                        node.factory.clone(),
                        node.service.clone(),
                        node.mailbox_capacity,
                        node.timeout,
                        crate::park::ParkGate::new(node.park_tx.subscribe()),
                        node.stats.clone(),
                    ))
                }
            }
        };
        let Some((name, factory, wiring, capacity, timeout, gate, stats)) = decision else {
            return false;
        };
        self.restarts_total += 1;
        metrics::counter!("candyriya_services_restarts_total").increment(1);
        let (mailbox, body) = build_run(wiring, capacity, factory, pid);
        let service_id = crate::runner::spawn_wrapper(
            service,
            Arc::clone(tree),
            pid,
            &name,
            body,
            crate::runner::BodyWire {
                park: gate,
                conn: stats,
                timeout,
            },
        );
        if let Some(node) = self.nodes.get_mut(&pid) {
            node.mailbox = Some(mailbox);
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
        crate::metrics::describe();
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

    /// Crate-private access to the registry for sibling modules.
    ///
    /// Callers must never hold the guard across an await; it is used the
    /// same way everywhere: one short critical section, then release.
    pub(crate) fn lock_state(&self) -> parking_lot::MutexGuard<'_, TreeState> {
        self.inner.lock()
    }

    /// Spawn a unit as a child of `parent`.
    ///
    /// The parent must exist and be `Running` — a zombie cannot supervise
    /// anything, and pid reuse does not exist. The body is created
    /// immediately and runs through the central task service; the tree only
    /// tracks its lifecycle. Body factories are invoked synchronously at
    /// registration time so the unit's mailbox is armed before any caller
    /// can resolve it; a panicking factory aborts the spawn instead of
    /// being misreported as a body exit.
    ///
    /// Grants listed in [`UnitSpec::with_grant`] are installed only after
    /// the whole spawn succeeds; a spawn that fails for any reason installs
    /// nothing.
    ///
    /// # Errors
    ///
    /// - [`InitError::NoSuchParent`] — no unit with pid `parent` exists.
    /// - [`InitError::ParentNotRunning`] — the parent is not (or no longer) running.
    /// - [`InitError::InvalidName`] — the name is empty, over 128 bytes or
    ///   contains control characters.
    /// - [`InitError::NoSuchUnit`] — a grant in the spec points at a unit
    ///   that does not exist.
    /// - [`InitError::GrantNotHeld`] — a grant in the spec is not passable
    ///   by `parent`: it is neither an ancestor of the grant target nor
    ///   itself holds a grant to it.
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
        // grants are validated BEFORE the pid is allocated and before any
        // side effect exists: a failed spawn must leave the tree untouched.
        // A handle can only be passed down by a parent that is an ancestor
        // of the target (subtree rule) or itself holds one — a unit can
        // never mint a handle to something it doesn't already reach.
        for &grant in &spec.grants {
            if !st.nodes.contains_key(&grant) {
                return Err(InitError::NoSuchUnit(grant));
            }
            let legal = st.is_ancestor(parent, grant) || st.has_grant(parent, grant);
            if !legal {
                return Err(InitError::GrantNotHeld {
                    parent,
                    target: grant,
                });
            }
        }
        let pid = Pid::new(st.next_raw_pid);
        st.next_raw_pid += 1;
        let capacity = spec.mailbox_capacity.unwrap_or(DEFAULT_MAILBOX_CAPACITY);
        let (park_tx, park_rx) = watch::channel(false);
        // connection units get counters bound at spawn; everyone else pays a None
        let conn_stats = (spec.kind == UnitKind::Connection).then(crate::stats::ConnStats::new);
        let (mailbox, body) = build_run(spec.service.clone(), capacity, spec.factory.clone(), pid);
        // The tree lock is held across the spawn: the wrapper can never run
        // before the node exists (a fast exit would otherwise race the
        // insert and lose its status), and a concurrent cancel() blocks
        // until the unit is fully registered. The body factory is called
        // synchronously so the mailbox is armed before any caller can
        // resolve the sender — a call made right after spawn never sees a
        // half-registered target.
        let service_id = crate::runner::spawn_wrapper(
            &self.service,
            Arc::clone(&self.inner),
            pid,
            &spec.name,
            body,
            crate::runner::BodyWire {
                park: crate::park::ParkGate::new(park_rx),
                conn: conn_stats.clone(),
                timeout: spec.timeout,
            },
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
                mailbox: Some(mailbox),
                mailbox_capacity: capacity,
                service_kind: spec.service_kind,
                call_timeout: spec.call_timeout,
                service: spec.service.clone(),
                park_tx,
                parked: false,
                stats: conn_stats,
            },
        );
        for &grant in &spec.grants {
            st.insert_grant(pid, grant);
        }
        Ok(pid)
    }

    /// Adopt a unit whose work already completed before the tree existed.
    ///
    /// The loader phase — CLI arguments, telemetry, the configuration read,
    /// address resolution — happens before any [`Init`] exists, so those
    /// steps can never be [`Init::spawn_unit`]s. Registering them as they
    /// finish keeps the tree the source of truth for what ran, in what
    /// order and with what outcome: the unit is born as a `Zombie` carrying
    /// `status`, and is harvested by the ordinary reaping path
    /// ([`Init::reap_children`]). Adoption runs no body and never will;
    /// the outcome is frozen at birth and the ring keeps the record like
    /// any other exit.
    ///
    /// This exists for the bootstrap phase only, by design. A unit adopted
    /// this way never runs again, cannot be retried and cannot be stopped
    /// mid-body — adopting work *after* the tree is up would silently
    /// defeat supervision, so anything the tree can spawn must go through
    /// [`Init::spawn_unit`] instead. If you reach for this mid-run, you
    /// want `spawn_unit`.
    ///
    /// The adopted unit's `started_at` and `exited_at` are both stamped at
    /// adoption time: the loader does not track per-step instants, and
    /// inventing them would fabricate measurements nobody took. The
    /// adopted unit's restart count is always 0 and its restart policy is
    /// always `Never`.
    ///
    /// # Errors
    ///
    /// - [`InitError::NoSuchParent`] — no unit with pid `parent` exists.
    /// - [`InitError::ParentNotRunning`] — `parent` exists but is not
    ///   `Running`, so it cannot supervise the adopted unit.
    /// - [`InitError::InvalidName`] — the name violates the naming contract.
    pub fn adopt_unit(
        &self,
        parent: Pid,
        name: impl Into<String>,
        kind: UnitKind,
        status: ExitStatus,
    ) -> Result<Pid, InitError> {
        let name = name.into();
        validate_name(&name)?;
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
        let now = Instant::now();
        let record = ExitRecord {
            pid,
            ppid: parent,
            name: name.clone(),
            kind,
            status: status.clone(),
            restart_count: 0,
            started_at: now,
            exited_at: now,
        };
        // the unit is born as a zombie: the receiver is dropped immediately
        // (calls fail fast with `NotRunning`, like plain units) and nothing
        // ever runs against the node
        let (mailbox, dead_rx) = mpsc::channel(DEFAULT_MAILBOX_CAPACITY);
        drop(dead_rx);
        let (park_tx, park_rx) = watch::channel(false);
        drop(park_rx);
        st.zombies_current += 1;
        st.exits.push(record);
        st.nodes.insert(
            pid,
            UnitNode {
                pid,
                ppid: parent,
                name,
                kind,
                state: UnitState::Zombie,
                started_at: now,
                restart_count: 0,
                last_exit: Some(status),
                exited_at: Some(now),
                policy: RestartPolicy::Never,
                factory: Arc::new(|| Box::pin(std::future::pending::<ExitStatus>())),
                timeout: None,
                attempts: AttemptWindow::new(),
                stop_requested: false,
                service_id: None,
                mailbox: Some(mailbox),
                mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
                service_kind: None,
                call_timeout: None,
                service: None,
                park_tx,
                parked: false,
                // adopted units never run bodies; no counters
                stats: None,
            },
        );
        st.emit_zombie_metrics();
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

    /// Freeze a unit cooperatively: flip its park gate closed.
    ///
    /// A conforming body parks at its next loop boundary and stays parked
    /// until [`Init::unfreeze`] — no reads, no frame processing, no torn
    /// await; TCP backpressure builds naturally. This is the operator's
    /// freeze, not a signal: units that never check the gate (bootstrap
    /// glue, one-shot steps) are unaffected, which is the honest contract
    /// of a cooperative gate. Flipping the gate of a unit that already
    /// exited is accepted as a no-op on the runtime side (the flag still
    /// lands, like [`Init::cancel`]'s does).
    ///
    /// Returns whether the gate changed state: `false` when the unit was
    /// already frozen.
    ///
    /// # Errors
    ///
    /// [`InitError::NoSuchUnit`] — no unit with pid `pid` exists.
    pub fn freeze(&self, pid: Pid) -> Result<bool, InitError> {
        self.set_parked(pid, true)
    }

    /// Unfreeze a unit: flip its park gate open.
    ///
    /// A parked body resumes at its parked await. Returns whether the gate
    /// changed state: `false` when the unit was already running.
    ///
    /// # Errors
    ///
    /// [`InitError::NoSuchUnit`] — no unit with pid `pid` exists.
    pub fn unfreeze(&self, pid: Pid) -> Result<bool, InitError> {
        self.set_parked(pid, false)
    }

    /// Whether the unit is currently parked (see [`Init::freeze`]).
    ///
    /// # Errors
    ///
    /// [`InitError::NoSuchUnit`] — no unit with pid `pid` exists.
    pub fn is_parked(&self, pid: Pid) -> Result<bool, InitError> {
        let st = self.inner.lock();
        let node = st.nodes.get(&pid).ok_or(InitError::NoSuchUnit(pid))?;
        Ok(node.parked)
    }

    /// Flip a unit's park gate, updating the node's bookkeeping and the
    /// gate in one critical section.
    ///
    /// The watch sender never blocks and never fails here — `send_replace`
    /// works with zero receivers (a parked-until-reaped body may already be
    /// gone), so a flip on a zombie is recorded, not reported.
    fn set_parked(&self, pid: Pid, parked: bool) -> Result<bool, InitError> {
        let mut st = self.inner.lock();
        let node = st.nodes.get_mut(&pid).ok_or(InitError::NoSuchUnit(pid))?;
        let changed = node.parked != parked;
        node.parked = parked;
        let _ = node.park_tx.send_replace(parked);
        if changed {
            tracing::info!(%pid, parked, "unit park gate flipped");
        }
        Ok(changed)
    }

    /// Cancel every non-root unit in the tree, newest first (reverse spawn
    /// order), and return the pids it actually stopped.
    ///
    /// One code path for "stop everything": the shutdown cascade and the
    /// boot abort both want this, and doing it in the kernel guarantees
    /// every non-root unit is reached — including units spawned after the
    /// caller last looked at the registry. The root (pid 1) is skipped:
    /// its lifecycle is the process's.
    ///
    /// Ordering is descending pid, which is reverse spawn order for every
    /// unit the tree registered: pids are never reused, so a higher pid is
    /// always a younger unit. Tearing down newest-first means children
    /// finish before their parents notice, which keeps re-parenting to the
    /// root meaningful at the end of the process.
    ///
    /// Only units whose [`Init::cancel`] reported an actual stop request
    /// are returned — units already finished (zombies, failed-terminal)
    /// still get their stop flag lands but are not listed. Callers that
    /// need the whole tree gone still have to reap (see
    /// [`Init::reap_children`]).
    ///
    /// Cancellation itself is unit-local: a unit dies at its next await,
    /// not synchronously. Like [`Init::cancel`], this flip is instant and
    /// total; waiting for the exits is the caller's job.
    pub fn cancel_all(&self) -> Vec<Pid> {
        let mut pids: Vec<Pid> = {
            let st = self.inner.lock();
            st.nodes
                .keys()
                .copied()
                .filter(|&pid| pid != Pid::INIT)
                .collect()
        };
        pids.sort_unstable_by(|a, b| b.cmp(a));
        let mut swept = Vec::with_capacity(pids.len());
        for pid in pids {
            // a unit that vanished between the snapshot and the flip (or was
            // already finished) reports false and is not listed
            if matches!(self.cancel(pid), Ok(true)) {
                swept.push(pid);
            }
        }
        swept
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

    /// Snapshot of a single connection unit, if it exists.
    ///
    /// Returns `None` for a missing pid and for units that carry no
    /// counters (anything that is not a [`UnitKind::Connection`] unit), so
    /// callers never have to special-case kinds themselves.
    #[must_use]
    pub fn connection_info(&self, pid: Pid) -> Option<ConnectionInfo> {
        let st = self.inner.lock();
        st.nodes.get(&pid).and_then(conn_info)
    }

    /// Snapshot of every connection unit, ascending pid order.
    ///
    /// Ordering is spawn order, deliberately: ps's top-N by traffic is a
    /// consumer-side sort (N is small, and the honest ordering — by what a
    /// human asked for — belongs to the view, not the kernel). Zombie
    /// connection units keep their final counters for the brief reap
    /// window, so a ps table never shows half-torn numbers.
    #[must_use]
    pub fn connections(&self) -> Vec<ConnectionInfo> {
        let st = self.inner.lock();
        let mut infos: Vec<ConnectionInfo> = st.nodes.values().filter_map(conn_info).collect();
        infos.sort_by_key(|info| info.pid);
        infos
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

/// Build the read model for a connection unit, if the node carries
/// counters. The registry stays the source of truth; the model owns
/// nothing.
fn conn_info(node: &UnitNode) -> Option<ConnectionInfo> {
    let stats = node.stats.as_ref()?;
    let (bytes_rx, bytes_tx, connected_at, last_activity) = stats.snapshot();
    Some(ConnectionInfo {
        pid: node.pid,
        name: node.name.clone(),
        state: node.state,
        parked: node.parked,
        connected_at,
        bytes_rx,
        bytes_tx,
        last_activity,
    })
}

/// Build one body run's mailbox and body.
///
/// One code path for spawn and restart. Units with a service handler rebuild
/// both halves from the stored wiring; plain units get a mailbox whose
/// receiver is dropped at once (calls fail fast with `NotRunning`, since
/// `try_send` reports the channel closed). The body factory is invoked
/// synchronously here — before the unit is registered and before any caller
/// can resolve the sender — so a call issued right after spawn never sees a
/// half-armed target.
///
/// A factory panic surfaces at the spawn/restart site rather than inside the
/// task service. That is deliberate: factories are construction closures,
/// and pretending a construction failure is a body exit would misreport it
/// as `Panicked` in the exit ring.
fn build_run(
    wiring: Option<Arc<dyn ServiceWiring>>,
    capacity: usize,
    factory: UnitBodyFactory,
    pid: Pid,
) -> (mpsc::Sender<Envelope>, UnitBody) {
    match wiring {
        Some(wiring) => wiring.build(pid, capacity),
        None => {
            let (tx, rx) = mpsc::channel(capacity);
            drop(rx);
            (tx, factory())
        }
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
