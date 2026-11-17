//! Scoped (plugin) task service with hard quotas.
//!
//! Note on isolation, because it is easy to misread: WASM plugin calls run
//! on the plugin's own OS thread behind a mailbox (see the plugin crate).
//! Scoped tasks cover the *other* things a plugin asks the scheduler to do
//! for it — timers, intervals, recurring jobs, blocking work — and they run
//! on the core machinery with deliberately tight quotas and
//! [`TaskPriority::Low`]. Core subsystems are never blocked on a plugin
//! scope, and a scope can never hold a core priority.
//!
//! Accounting model, so the numbers make sense: a submission is registered
//! in the global registry first (that is what makes every task findable by
//! `cancel` and `drain`), then admission is decided under the scope lock.
//! On rejection the already-registered entry is consumed as cancelled.
//! Scope counters only count tasks the scope admitted; the queue itself is
//! a `VecDeque` of pinned futures drained under the same lock, so there is
//! no window in which a parked task is invisible to `close` or `drain`.

use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::time::Instant;

use parking_lot::Mutex;
use tokio::sync::watch;

use crate::error::ScopeError;
use crate::service::{Outcome, Shared, TaskEntry, TaskKind};
use crate::task::{TaskId, TaskPriority};

/// Hard limits for a plugin scope.
///
/// The defaults match the documented plugin quotas: 4 concurrent tasks,
/// 16 queued, 5 second timeout, low priority.
#[derive(Clone, Debug)]
pub struct ScopeLimits {
    /// Maximum number of tasks running at once. Further submissions are
    /// queued, up to [`ScopeLimits::max_queued`]. Defaults to 4.
    pub max_concurrent: u32,
    /// Maximum number of tasks waiting for a slot. Submissions beyond this
    /// fail with [`ScopeError::QuotaExceeded`]. Defaults to 16.
    pub max_queued: u32,
    /// Maximum wall-clock time a running task may take. On expiry the
    /// future is dropped (its destructors run) and the task is accounted
    /// for as timed out. Cooperative: the timeout starts when the task
    /// starts running, not while it waits in the queue. Defaults to 5s.
    /// Does not apply to [`ScopedTaskService::submit_blocking`].
    pub task_timeout: Duration,
    /// Priority of scope tasks. Always coerced to [`TaskPriority::Low`] at
    /// submission time; setting anything else has no effect and logs a
    /// warning at scope creation. Defaults to [`TaskPriority::Low`].
    pub priority: TaskPriority,
}

impl Default for ScopeLimits {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            max_queued: 16,
            task_timeout: Duration::from_secs(5),
            priority: TaskPriority::Low,
        }
    }
}

/// Point-in-time statistics for a scope.
///
/// `active` and `queued` are instantaneous; the rest are cumulative since
/// the scope was created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeStats {
    /// Tasks currently running in the scope.
    pub active: u32,
    /// Tasks waiting for a slot in the scope.
    pub queued: u32,
    /// Tasks that completed normally, in total.
    pub completed: u64,
    /// Tasks that panicked, in total.
    pub panicked: u64,
    /// Tasks that exceeded [`ScopeLimits::task_timeout`], or were still
    /// running when the service drained, in total.
    pub timed_out: u64,
    /// Tasks cancelled (including scope closure), in total.
    pub cancelled: u64,
}

/// The payload parked in the wait queue.
enum QueuePayload {
    /// Async body.
    Async(Pin<Box<dyn Future<Output = ()> + Send>>),
    /// Blocking closure, dispatched to the rayon pool when run.
    Blocking(Box<dyn FnOnce() + Send>),
}

impl std::fmt::Debug for QueuePayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Async(_) => f.write_str("Async(..)"),
            Self::Blocking(_) => f.write_str("Blocking(..)"),
        }
    }
}

/// A task waiting for a slot.
struct QueuedTask {
    id: TaskId,
    name: String,
    cancel_rx: watch::Receiver<bool>,
    payload: QueuePayload,
}

#[derive(Default)]
struct ScopeState {
    closed: bool,
    running: u32,
    running_ids: HashSet<TaskId>,
    queued_ids: HashSet<TaskId>,
    queue: VecDeque<QueuedTask>,
    completed: u64,
    panicked: u64,
    timed_out: u64,
    cancelled: u64,
}

impl ScopeState {
    fn bump(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Completed => self.completed += 1,
            Outcome::Cancelled => self.cancelled += 1,
            Outcome::Panicked => self.panicked += 1,
            Outcome::TimedOut => self.timed_out += 1,
        }
    }
}

/// Internal state shared by everything that touches a scope.
pub(crate) struct ScopeShared {
    name: String,
    limits: ScopeLimits,
    /// Shared with the core service; when `false` (drain), the queue never issues.
    accepting: Arc<AtomicBool>,
    /// Path back into the global service.
    inner: Arc<Shared>,
    state: Mutex<ScopeState>,
}

impl ScopeShared {
    /// Finalize bookkeeping for a task leaving the scope; issue queued work
    /// if a slot opened and neither the scope nor the whole service is
    /// shutting down.
    pub(crate) fn on_finish(self: &Arc<Self>, id: TaskId, outcome: Outcome) {
        let to_spawn: Vec<QueuedTask> = {
            let mut st = self.state.lock();
            let was_running = st.running_ids.remove(&id);
            let was_queued = !was_running && st.queued_ids.remove(&id);
            if was_running {
                st.running = st.running.saturating_sub(1);
            } else if was_queued && let Some(pos) = st.queue.iter().position(|q| q.id == id) {
                // the parked future is dropped unexecuted here
                st.queue.remove(pos);
            }
            // rejection-finalized entries are not known to the scope; they
            // are counted in the global totals but not here
            let known = was_running || was_queued;
            if known {
                st.bump(outcome);
            }

            let shutting_down = st.closed || !self.accepting.load(Ordering::Acquire);
            if shutting_down {
                // release the whole queue; every parked task is dropped
                // unexecuted and accounted for with this same outcome
                let drained: Vec<QueuedTask> = st.queue.drain(..).collect();
                for q in drained {
                    st.queued_ids.remove(&q.id);
                    st.bump(outcome);
                }
                return; // never issues new work while shutting down
            }
            if !known {
                return;
            }
            let mut spawnable = Vec::new();
            while st.running < self.limits.max_concurrent {
                match st.queue.pop_front() {
                    None => break,
                    Some(q) => {
                        st.queued_ids.remove(&q.id);
                        if *q.cancel_rx.borrow() {
                            // cancelled while parked; drop unexecuted
                            st.bump(Outcome::Cancelled);
                            continue;
                        }
                        st.running += 1;
                        st.running_ids.insert(q.id);
                        spawnable.push(q);
                        break;
                    }
                }
            }
            spawnable
        };
        for q in to_spawn {
            q.spawn(self);
        }
    }
}

impl QueuedTask {
    /// Spawn the wrapper for a dequeued task.
    #[allow(
        clippy::disallowed_methods,
        reason = "this is the kernel itself; its rayon pool is the sanctioned blocking pool"
    )]
    fn spawn(self, scope: &Arc<ScopeShared>) {
        let QueuedTask {
            id,
            name,
            mut cancel_rx,
            payload,
        } = self;
        match payload {
            QueuePayload::Async(fut) => {
                let timeout = scope.limits.task_timeout;
                let wrapper = async move {
                    tokio::select! {
                        biased;
                        _ = cancel_rx.changed() => Outcome::Cancelled,
                        outcome = tokio::time::timeout(timeout, fut) => match outcome {
                            Ok(()) => Outcome::Completed,
                            Err(_) => Outcome::TimedOut,
                        }
                    }
                };
                scope.inner.launch(id, name, wrapper);
            }
            QueuePayload::Blocking(f) => {
                let (done_tx, done_rx) = tokio::sync::oneshot::channel::<Outcome>();
                scope.inner.rayon.spawn(move || {
                    // blocking closures are not interrupted by the timeout;
                    // the slot is held until the closure returns
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
                scope.inner.launch(id, name, wrapper);
            }
        }
    }
}

/// A plugin-scoped view of the task service.
///
/// Obtained from [`crate::TaskService::scoped`]. Cheap to clone; clones
/// share the same scope state.
///
/// # Isolation and quotas
///
/// - At most [`ScopeLimits::max_concurrent`] tasks run at once.
/// - Further submissions wait in a bounded queue of at most
///   [`ScopeLimits::max_queued`] entries; beyond that, submission fails
///   with [`ScopeError::QuotaExceeded`]. Rejection is an error, never a
///   silent drop or an unbounded stall, so plugin code is expected to shed
///   load.
/// - A running async task is dropped (timed out) after
///   [`ScopeLimits::task_timeout`].
/// - Tasks always run at [`TaskPriority::Low`];
///   [`ScopeLimits::priority`] can never elevate them.
/// - [`ScopedTaskService::close`] cancels every task the scope registered,
///   queued or running.
///
/// # Panics
///
/// Submissions are rejected, never panicked, on quota exhaustion, after
/// close, or once the whole service is draining.
#[derive(Clone)]
pub struct ScopedTaskService {
    inner: Arc<Shared>,
    shared: Arc<ScopeShared>,
}

impl ScopedTaskService {
    pub(crate) fn new(
        inner: Arc<Shared>,
        accepting: Arc<AtomicBool>,
        scope: &str,
        limits: ScopeLimits,
    ) -> Self {
        let shared = Arc::new(ScopeShared {
            name: scope.to_string(),
            limits,
            accepting,
            inner: inner.clone(),
            state: Mutex::new(ScopeState::default()),
        });
        if shared.limits.priority != TaskPriority::Low {
            tracing::warn!(
                scope = %scope,
                requested = ?shared.limits.priority,
                "scoped tasks always run at Low priority; requested priority ignored"
            );
        }
        Self { inner, shared }
    }

    /// The scope's name, as passed to [`crate::TaskService::scoped`].
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// The limits the scope was created with.
    pub fn limits(&self) -> &ScopeLimits {
        &self.shared.limits
    }

    /// Submit an async task to the scope.
    ///
    /// Returns the task id on success. If the scope's concurrent slots are
    /// all taken, the task waits in the bounded queue; if the queue is also
    /// full, submission fails with [`ScopeError::QuotaExceeded`].
    pub fn submit<Fut>(
        &self,
        name: impl Into<Cow<'static, str>>,
        fut: Fut,
    ) -> Result<TaskId, ScopeError>
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.submit_payload(name, QueuePayload::Async(Box::pin(fut)))
    }

    /// Submit a blocking job to the scope.
    ///
    /// The job runs on the service's rayon pool. [`ScopeLimits::task_timeout`]
    /// does not apply — a rayon closure cannot be interrupted — so the slot
    /// is held until the closure returns. Panics are contained and counted.
    pub fn submit_blocking<F>(
        &self,
        name: impl Into<Cow<'static, str>>,
        f: F,
    ) -> Result<TaskId, ScopeError>
    where
        F: FnOnce() + Send + 'static,
    {
        self.submit_payload(name, QueuePayload::Blocking(Box::new(f)))
    }

    fn submit_payload(
        &self,
        name: impl Into<Cow<'static, str>>,
        payload: QueuePayload,
    ) -> Result<TaskId, ScopeError> {
        let name = name.into().into_owned();
        let scope_name = self.shared.name.clone();

        // stage 1: register in the global registry so cancel/drain can find
        // the task no matter what the scope decides afterwards
        let id = self.inner.alloc();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let registered = self.inner.register(
            id,
            TaskEntry {
                name: name.clone(),
                // scope tasks are always Low, no matter what limits claim
                priority: TaskPriority::Low,
                started_at: Instant::now(),
                kind: TaskKind::Scope,
                cancel: cancel_tx,
                cron_uuid: None,
                scope: Some(self.shared.clone()),
            },
        );
        if !registered {
            // service is draining; the entry was already finalized as
            // cancelled inside register
            return Err(ScopeError::Draining(scope_name));
        }

        // stage 2: admission under the scope lock
        enum Admit {
            Queued,
            Running {
                name: String,
                cancel_rx: watch::Receiver<bool>,
                payload: QueuePayload,
            },
        }
        let admit = {
            let mut st = self.shared.state.lock();
            if st.closed {
                // close() took the registration lists already; consume our
                // own entry now instead of leaving it orphaned
                drop(st);
                let _ = self.inner.cancel(id);
                return Err(ScopeError::Closed(scope_name));
            }
            if st.running >= self.shared.limits.max_concurrent {
                if st.queued_ids.len() >= self.shared.limits.max_queued as usize {
                    // rejected at admission; consume the entry
                    let err = ScopeError::QuotaExceeded {
                        running: st.running,
                        running_max: self.shared.limits.max_concurrent,
                        queued: st.queued_ids.len() as u32,
                        queued_max: self.shared.limits.max_queued,
                    };
                    drop(st);
                    let _ = self.inner.cancel(id);
                    return Err(err);
                }
                st.queue.push_back(QueuedTask {
                    id,
                    name: name.clone(),
                    cancel_rx,
                    payload,
                });
                st.queued_ids.insert(id);
                Admit::Queued
            } else {
                st.running += 1;
                st.running_ids.insert(id);
                Admit::Running {
                    name,
                    cancel_rx,
                    payload,
                }
            }
        };

        match admit {
            Admit::Queued => Ok(id),
            Admit::Running {
                name,
                cancel_rx,
                payload,
            } => {
                // if the scope closed during registration, the wrapper's
                // biased select still picks the cancel arm first — the
                // future body never executes
                QueuedTask {
                    id,
                    name,
                    cancel_rx,
                    payload,
                }
                .spawn(&self.shared);
                Ok(id)
            }
        }
    }

    /// Cancel every task in the scope, queued or running.
    ///
    /// Idempotent. The scope accepts no further work afterwards; submissions
    /// fail with [`ScopeError::Closed`]. Parked tasks are dropped unexecuted
    /// and counted as cancelled; already-running async tasks finish at their
    /// next await point; blocking closures run to completion.
    pub fn close(&self) {
        let ids: Vec<TaskId> = {
            let mut st = self.shared.state.lock();
            if st.closed {
                return;
            }
            st.closed = true;
            st.running_ids
                .iter()
                .chain(st.queued_ids.iter())
                .copied()
                .collect()
        };
        // the registry knows every one of these ids; cancel flips the
        // token, removes the entry and routes the accounting through
        // `on_finish`, which drops parked futures unexecuted
        for id in ids {
            let _ = self.inner.cancel(id);
        }
    }

    /// Snapshot the scope's counters.
    pub fn stats(&self) -> ScopeStats {
        let st = self.shared.state.lock();
        ScopeStats {
            active: st.running,
            queued: st.queued_ids.len() as u32,
            completed: st.completed,
            panicked: st.panicked,
            timed_out: st.timed_out,
            cancelled: st.cancelled,
        }
    }
}
