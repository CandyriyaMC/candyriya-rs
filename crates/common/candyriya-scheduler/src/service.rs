//! The core task service: spawning, blocking offload, delays, intervals,
//! cancellation, statistics and the graceful drain.
//!
//! Concurrency model, in one paragraph: each task is a wrapper future pinned
//! to the service's `tokio` runtime. The wrapper selects between the inner
//! future and a `watch::Receiver` cancellation token; a wrapper that resolves
//! (for any reason, including a panicking inner future) reports its outcome
//! to a supervisor task, which is the *single* finalization point for every
//! task. `finish` is idempotent on already-removed entries, so tasks
//! finalized early by [`TaskService::cancel`] or [`TaskService::drain`]
//! simply no-op when their wrapper reports later. Long-lived recurrences
//! (cron entries) have no wrapper and are finalized by the cancelling party
//! instead. Nothing here uses `unsafe`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::watch;

use crate::error::BuildError;
use crate::scope::{ScopeLimits, ScopeShared, ScopedTaskService};
use crate::stats::{ShutdownReport, TaskStats};
use crate::task::{TaskId, TaskPriority};

/// Internal classification of a registered task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskKind {
    /// Spawned async task.
    Spawn,
    /// Offloaded blocking task on the rayon pool.
    Blocking,
    /// One-shot task that runs after a delay.
    Delay,
    /// Repeating task re-created from a factory on an interval.
    Every,
    /// Cron-scheduled recurring task.
    Cron,
    /// Plugin (scoped) task.
    Scope,
}

/// Internal outcome of a task wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The task body completed.
    Completed,
    /// The task was cancelled before it completed.
    Cancelled,
    /// The task body panicked.
    Panicked,
    /// The task exceeded its time limit.
    TimedOut,
}

/// Internal bookkeeping for a registered task.
pub(crate) struct TaskEntry {
    /// Human-readable name, cloneable for logging.
    pub(crate) name: String,
    /// Priority hint stored on the task.
    pub(crate) priority: TaskPriority,
    /// When the task was registered.
    pub(crate) started_at: Instant,
    /// Internal classification of the task.
    pub(crate) kind: TaskKind,
    /// Cancellation token; flip to `true` to request cancellation.
    pub(crate) cancel: watch::Sender<bool>,
    /// Cron job id, if this entry fronts a cron job.
    pub(crate) cron_uuid: Option<uuid::Uuid>,
    /// Hook into the owning scope, for plugin (scoped) tasks.
    pub(crate) scope: Option<Arc<ScopeShared>>,
}

/// Accumulated counters shared across the whole service.
#[derive(Default)]
struct Totals {
    completed: u64,
    cancelled: u64,
    panicked: u64,
    timed_out: u64,
    total_duration: Duration,
}

impl Totals {
    fn bump(&mut self, outcome: Outcome, duration: Duration) {
        match outcome {
            Outcome::Completed => self.completed += 1,
            Outcome::Cancelled => self.cancelled += 1,
            Outcome::Panicked => self.panicked += 1,
            Outcome::TimedOut => self.timed_out += 1,
        }
        self.total_duration += duration;
    }
}

/// Shared state behind every clone of [`TaskService`].
pub(crate) struct Shared {
    /// The runtime that drives async tasks, supervisors and the cron scheduler.
    pub(crate) handle: tokio::runtime::Handle,
    /// Fixed-size rayon pool for blocking work.
    pub(crate) rayon: rayon::ThreadPool,
    /// Cleared by `drain`; gates all new registrations.
    pub(crate) accepting: Arc<AtomicBool>,
    /// Registered tasks, keyed by id.
    pub(crate) tasks: Mutex<HashMap<TaskId, TaskEntry>>,
    /// Allocator for [`TaskId`].
    next_id: AtomicU64,
    /// Accumulated counters.
    totals: Mutex<Totals>,
    /// Lazily created cron scheduler; created on the first `cron` call.
    pub(crate) cron:
        Arc<tokio::sync::OnceCell<tokio::sync::Mutex<tokio_cron_scheduler::JobScheduler>>>,
}

impl Shared {
    /// Bump the global panic counter; used by cron ticks and blocking
    /// wrappers where no per-task outcome exists.
    pub(crate) fn record_panic(&self) {
        self.totals.lock().panicked += 1;
    }

    fn allocate(&self) -> TaskId {
        TaskId::new(self.next_id.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Allocate an id without registering anything. The caller must follow
    /// up with exactly one [`Shared::register`]. Pairs with `alloc` for the
    /// scoped admission path, where the decision to enqueue or run is made
    /// before the entry can be inserted.
    pub(crate) fn alloc(&self) -> TaskId {
        self.allocate()
    }

    /// Register a pre-allocated id, closing the registration race against
    /// `drain`: the accepting flag is checked under the same lock that
    /// inserts into the map. Post-drain registrations are finalized as
    /// cancelled immediately and their bodies are never executed.
    /// Returns true if accepted.
    pub(crate) fn register(self: &Arc<Self>, id: TaskId, entry: TaskEntry) -> bool {
        let mut map = self.tasks.lock();
        if self.accepting.load(Ordering::Acquire) {
            map.insert(id, entry);
            true
        } else {
            drop(map);
            // finalize outside the lock; the task body is never executed
            self.finalize_entry(id, entry, Outcome::Cancelled);
            false
        }
    }

    fn finalize_entry(self: &Arc<Self>, id: TaskId, entry: TaskEntry, outcome: Outcome) {
        let duration = entry.started_at.elapsed();
        {
            let mut totals = self.totals.lock();
            totals.bump(outcome, duration);
        }
        if let Some(cron_uuid) = entry.cron_uuid {
            // cron entries exit exclusively via cancel/close/drain; detach
            // the cron job so it stops ticking
            let cron = self.cron.clone();
            let handle = self.handle.clone();
            handle.spawn(async move {
                if let Some(sched) = cron.get()
                    && let Err(e) = sched.lock().await.remove(&cron_uuid).await
                {
                    tracing::warn!(%cron_uuid, "failed to remove cron job: {e}");
                }
            });
        }
        if let Some(scope) = entry.scope {
            scope.on_finish(id, outcome);
        }
    }

    /// Finalize a task by id; no-op if it is already gone (idempotent).
    pub(crate) fn finish(self: &Arc<Self>, id: TaskId, outcome: Outcome) {
        let entry = self.tasks.lock().remove(&id);
        if let Some(entry) = entry {
            self.finalize_entry(id, entry, outcome);
        }
    }

    /// Flip a task's cancel token, removing it from the registry and
    /// finalizing it as cancelled. Returns false if the task was already
    /// gone (completed, panicked, or already cancelled).
    pub(crate) fn cancel(self: &Arc<Self>, id: TaskId) -> bool {
        let entry = self.tasks.lock().remove(&id);
        let Some(entry) = entry else {
            return false;
        };
        let _ = entry.cancel.send(true);
        self.finalize_entry(id, entry, Outcome::Cancelled);
        true
    }

    /// Spawn a supervisor that observes the wrapper's outcome exactly once.
    fn supervise(
        self: &Arc<Self>,
        id: TaskId,
        name: String,
        join: tokio::task::JoinHandle<Outcome>,
    ) {
        let shared = self.clone();
        self.handle.spawn(async move {
            let outcome = match join.await {
                Ok(o) => o,
                Err(e) if e.is_panic() => {
                    tracing::error!(task = %name, %id, "task panicked: {e}");
                    Outcome::Panicked
                }
                Err(e) => {
                    tracing::warn!(task = %name, %id, "task aborted before completion: {e}");
                    Outcome::Cancelled
                }
            };
            shared.finish(id, outcome);
        });
    }

    /// Spawn a wrapper task plus its supervisor.
    pub(crate) fn launch(
        self: &Arc<Self>,
        id: TaskId,
        name: String,
        wrapper: impl Future<Output = Outcome> + Send + 'static,
    ) {
        let join = self.handle.spawn(wrapper);
        self.supervise(id, name, join);
    }

    /// Flip the cancel tokens of every registered task and finalize all of
    /// them as `TimedOut`. Used by `drain` at its deadline.
    fn finalize_all_timed_out(self: &Arc<Self>) -> u64 {
        let remaining: Vec<(TaskId, TaskEntry)> = std::mem::take(&mut *self.tasks.lock())
            .into_iter()
            .collect();
        let count = remaining.len() as u64;
        for (id, entry) in remaining {
            let _ = entry.cancel.send(true);
            self.finalize_entry(id, entry, Outcome::TimedOut);
        }
        count
    }
}

/// Builder for [`TaskService`].
///
/// The runtime is received, not created: the composition root builds the
/// multi-thread runtime and hands the service a [`tokio::runtime::Handle`].
///
/// # Example
///
/// ```no_run
/// use candyriya_scheduler::TaskService;
/// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).build()?;
/// let service = TaskService::builder()
///     .handle(rt.handle().clone())
///     .blocking_threads(4)
///     .build()?;
/// # Ok(()) }
/// ```
pub struct TaskServiceBuilder {
    handle: Option<tokio::runtime::Handle>,
    blocking_threads: Option<usize>,
}

impl Default for TaskServiceBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskServiceBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self {
            handle: None,
            blocking_threads: None,
        }
    }

    /// Runtime the service drives its tasks on.
    ///
    /// Defaults to [`tokio::runtime::Handle::try_current`], so building the
    /// service from inside an async context works without calling this.
    #[must_use]
    pub fn handle(mut self, handle: tokio::runtime::Handle) -> Self {
        self.handle = Some(handle);
        self
    }

    /// Number of threads in the fixed rayon blocking pool.
    ///
    /// Fixed from construction on purpose: a pool that silently sized itself
    /// from ambient conditions is how proxies end up with three blocking
    /// threads on a 96-core box holding up compression. Defaults to
    /// `num_cpus::get()`; values below 1 are clamped to 1.
    #[must_use]
    pub fn blocking_threads(mut self, blocking_threads: usize) -> Self {
        self.blocking_threads = Some(blocking_threads);
        self
    }

    /// Build the service.
    ///
    /// Returns an error if no handle was provided and none is active, or if
    /// the rayon pool cannot be created.
    pub fn build(self) -> Result<TaskService, BuildError> {
        let handle = match self.handle {
            Some(handle) => handle,
            None => tokio::runtime::Handle::try_current().map_err(|_| BuildError::NoRuntime)?,
        };
        let threads = self.blocking_threads.unwrap_or_else(num_cpus::get).max(1);
        let rayon = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("candyriya-blocking-{index}"))
            .build()
            .map_err(|e| BuildError::BlockingPool(e.to_string()))?;
        let inner = Arc::new(Shared {
            handle,
            rayon,
            accepting: Arc::new(AtomicBool::new(true)),
            tasks: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            totals: Mutex::new(Totals::default()),
            cron: Arc::new(tokio::sync::OnceCell::new()),
        });
        Ok(TaskService { inner })
    }
}

/// The central task service. Every clone is cheap and refers to the same
/// underlying registry, pools and counters.
///
/// # Lifecycle
///
/// The service starts accepting work immediately. [`TaskService::drain`]
/// stops it from accepting new tasks, waits for the registered ones to
/// finish, and reports what happened. After a drain, spawning returns ids
/// whose tasks are already cancelled (async entry points) or `Err`
/// (cron, scoped submission).
///
/// # Cancellation
///
/// Cancellation is cooperative: a token is flipped and async wrappers stop
/// at the next await point, running their destructors. Blocking closures
/// cannot be interrupted and run to completion in the background.
///
/// # Shutdown order
///
/// When the proxy shuts down, `drain(timeout)` is expected to run before any
/// other shutdown phase; nothing should be holding work the service hasn't
/// been told about by then.
#[derive(Clone)]
pub struct TaskService {
    pub(crate) inner: Arc<Shared>,
}

impl TaskService {
    /// Start building a [`TaskService`].
    pub fn builder() -> TaskServiceBuilder {
        TaskServiceBuilder::new()
    }

    /// Spawn an async task with the default [`TaskPriority::Normal`].
    ///
    /// The returned id can be passed to [`TaskService::cancel`]. After a
    /// [`TaskService::drain`] the task is registered as cancelled and never
    /// executes.
    pub fn spawn_named(
        &self,
        name: impl Into<std::borrow::Cow<'static, str>>,
        fut: impl Future<Output = ()> + Send + 'static,
    ) -> TaskId {
        self.spawn_named_with_priority(name, TaskPriority::Normal, fut)
    }

    /// Spawn an async task with an explicit priority.
    pub fn spawn_named_with_priority(
        &self,
        name: impl Into<std::borrow::Cow<'static, str>>,
        priority: TaskPriority,
        fut: impl Future<Output = ()> + Send + 'static,
    ) -> TaskId {
        let name = name.into().into_owned();
        let id = self.inner.alloc();
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let accepted = self.inner.register(
            id,
            TaskEntry {
                name: name.clone(),
                priority,
                started_at: Instant::now(),
                kind: TaskKind::Spawn,
                cancel: cancel_tx,
                cron_uuid: None,
                scope: None,
            },
        );
        if !accepted {
            return id;
        }
        let wrapper = async move {
            tokio::pin!(fut);
            tokio::select! {
                biased;
                _ = cancel_rx.changed() => Outcome::Cancelled,
                _ = &mut fut => Outcome::Completed,
            }
        };
        self.inner.launch(id, name, wrapper);
        id
    }

    /// Offload blocking work onto the rayon pool.
    ///
    /// Use this for CPU-bound or otherwise thread-blocking jobs (compression,
    /// cryptography, serializing a batch of packets). Calling it from an
    /// async context never blocks the calling worker.
    ///
    /// Cancellation flips the token but cannot interrupt the closure; the
    /// closure runs to completion on a pool thread. Panics are contained:
    /// the task is counted as panicked and the pool is unaffected.
    pub fn spawn_blocking(
        &self,
        name: impl Into<std::borrow::Cow<'static, str>>,
        f: impl FnOnce() + Send + 'static,
    ) -> TaskId {
        let name = name.into().into_owned();
        let inner = self.inner.clone();
        let id = inner.alloc();
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let accepted = inner.register(
            id,
            TaskEntry {
                name: name.clone(),
                priority: TaskPriority::Normal,
                started_at: Instant::now(),
                kind: TaskKind::Blocking,
                cancel: cancel_tx,
                cron_uuid: None,
                scope: None,
            },
        );
        if !accepted {
            return id;
        }
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<Outcome>();
        inner.rayon.spawn(move || {
            let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
                Ok(()) => Outcome::Completed,
                Err(_) => Outcome::Panicked,
            };
            let _ = done_tx.send(outcome);
        });
        let wrapper = async move {
            tokio::select! {
                biased;
                _ = cancel_rx.changed() => Outcome::Cancelled,
                outcome = async {
                    match done_rx.await {
                        Ok(outcome) => outcome,
                        Err(_) => Outcome::Panicked,
                    }
                } => outcome,
            }
        };
        inner.launch(id, name, wrapper);
        id
    }

    /// Run a future once, after a delay.
    ///
    /// If the task is cancelled during the delay the future never executes.
    pub fn delay(
        &self,
        name: impl Into<std::borrow::Cow<'static, str>>,
        duration: Duration,
        fut: impl Future<Output = ()> + Send + 'static,
    ) -> TaskId {
        let name = name.into().into_owned();
        let id = self.inner.alloc();
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let accepted = self.inner.register(
            id,
            TaskEntry {
                name: name.clone(),
                priority: TaskPriority::Normal,
                started_at: Instant::now(),
                kind: TaskKind::Delay,
                cancel: cancel_tx,
                cron_uuid: None,
                scope: None,
            },
        );
        if !accepted {
            return id;
        }
        let wrapper = async move {
            tokio::select! {
                biased;
                _ = cancel_rx.changed() => Outcome::Cancelled,
                _ = tokio::time::sleep(duration) => {
                    tokio::select! {
                        biased;
                        _ = cancel_rx.changed() => Outcome::Cancelled,
                        _ = fut => Outcome::Completed,
                    }
                }
            }
        };
        self.inner.launch(id, name, wrapper);
        id
    }

    /// Run a factory repeatedly, once per interval, until cancelled.
    ///
    /// `factory` is called to produce a fresh future for every tick; the
    /// future runs to completion before the next tick is scheduled. The
    /// first tick happens after one `interval`, not immediately. Zero
    /// intervals are clamped to 1 ms to avoid a busy loop.
    ///
    /// Ticks run sequentially; a tick that panics is contained (counter +
    /// error log) and ends the task rather than killing the service.
    pub fn every<F, Fut>(
        &self,
        name: impl Into<std::borrow::Cow<'static, str>>,
        interval: Duration,
        factory: F,
    ) -> TaskId
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let name = name.into().into_owned();
        let interval = interval.max(Duration::from_millis(1));
        let id = self.inner.alloc();
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let accepted = self.inner.register(
            id,
            TaskEntry {
                name: name.clone(),
                priority: TaskPriority::Normal,
                started_at: Instant::now(),
                kind: TaskKind::Every,
                cancel: cancel_tx,
                cron_uuid: None,
                scope: None,
            },
        );
        if !accepted {
            return id;
        }
        let mut factory = factory;
        let wrapper = async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel_rx.changed() => break Outcome::Cancelled,
                    _ = tokio::time::sleep(interval) => {}
                }
                let tick = factory();
                tokio::select! {
                    biased;
                    _ = cancel_rx.changed() => break Outcome::Cancelled,
                    _ = tick => {}
                }
            }
        };
        self.inner.launch(id, name, wrapper);
        id
    }

    /// Create a scoped (plugin) view of the service with hard quotas.
    ///
    /// The scope shares the same runtime and registry; what differs is
    /// admission (bounded concurrency + bounded queue), the forced
    /// [`TaskPriority::Low`], a per-task timeout and per-scope metrics.
    /// See [`ScopedTaskService`] for the full model.
    pub fn scoped(&self, scope: &str, limits: ScopeLimits) -> ScopedTaskService {
        ScopedTaskService::new(
            self.inner.clone(),
            self.inner.accepting.clone(),
            scope,
            limits,
        )
    }

    /// Cancel a task by id.
    ///
    /// Flips the cancel token and removes the task from the registry
    /// immediately: the task stops appearing in [`TaskService::stats`] right
    /// away, while its wrapper unwinds in the background. Cron entries are
    /// finalized here as well, and their jobs are removed from the
    /// underlying scheduler.
    ///
    /// Returns `false` if the task was already gone.
    pub fn cancel(&self, id: TaskId) -> bool {
        self.inner.cancel(id)
    }

    /// Snapshot the current statistics.
    pub fn stats(&self) -> TaskStats {
        let (active, by_name, by_priority) = {
            let map = self.inner.tasks.lock();
            let mut by_name: HashMap<String, u32> = HashMap::new();
            let mut by_priority: HashMap<TaskPriority, u32> = HashMap::new();
            for entry in map.values() {
                *by_name.entry(entry.name.clone()).or_default() += 1;
                *by_priority.entry(entry.priority).or_default() += 1;
            }
            (map.len() as u32, by_name, by_priority)
        };
        let totals = self.inner.totals.lock();
        let finished = totals.completed + totals.cancelled + totals.panicked + totals.timed_out;
        let avg_duration =
            (finished > 0).then(|| totals.total_duration / u32::try_from(finished).unwrap_or(1));
        TaskStats {
            active,
            completed: totals.completed,
            cancelled: totals.cancelled,
            panicked: totals.panicked,
            timed_out: totals.timed_out,
            by_name,
            by_priority,
            avg_duration,
        }
    }

    /// Cancel the perpetual kinds (interval watchers, cron entries). They
    /// can never complete on their own, so `drain` flips them at drain time
    /// and waits only for finite work.
    fn cancel_perpetual(&self) {
        let ids: Vec<TaskId> = {
            let map = self.inner.tasks.lock();
            map.iter()
                .filter(|(_, entry)| matches!(entry.kind, TaskKind::Every | TaskKind::Cron))
                .map(|(id, _)| *id)
                .collect()
        };
        for id in ids {
            let _ = self.inner.cancel(id);
        }
    }

    /// Stop accepting new tasks, wait for the active ones, then report.
    ///
    /// New registrations after the drain starts are finalized as cancelled
    /// immediately and never run; cron registrations fail with
    /// [`crate::ScheduleError::Draining`].
    ///
    /// Perpetual watchers (`every`, `cron`) are cancelled at the start of
    /// the drain: they can never finish on their own. Finite tasks get
    /// `timeout` to finish. At the deadline, every remaining registration is
    /// removed from the registry, its token flipped, and accounted for as
    /// `timed_out` in the report; final outcomes of detached blocking
    /// closures are not tracked further.
    ///
    /// `completed` and `cancelled` are deltas over the call itself. The
    /// drain is idempotent: calling it on an already drained service returns
    /// zero deltas.
    pub async fn drain(&self, timeout: Duration) -> ShutdownReport {
        let started = Instant::now();
        let before = {
            let totals = self.inner.totals.lock();
            (totals.completed, totals.cancelled)
        };
        self.inner.accepting.store(false, Ordering::Release);
        self.cancel_perpetual();
        if let Some(sched) = self.inner.cron.get()
            && let Err(e) = sched.lock().await.shutdown().await
        {
            tracing::warn!("cron scheduler shutdown failed: {e}");
        }
        loop {
            let active = self.inner.tasks.lock().len();
            if active == 0 {
                let totals = self.inner.totals.lock();
                return ShutdownReport {
                    completed: totals.completed.saturating_sub(before.0),
                    cancelled: totals.cancelled.saturating_sub(before.1),
                    timed_out: 0,
                };
            }
            if started.elapsed() >= timeout {
                let timed_out = self.inner.finalize_all_timed_out();
                let totals = self.inner.totals.lock();
                return ShutdownReport {
                    completed: totals.completed.saturating_sub(before.0),
                    cancelled: totals.cancelled.saturating_sub(before.1),
                    timed_out,
                };
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}
