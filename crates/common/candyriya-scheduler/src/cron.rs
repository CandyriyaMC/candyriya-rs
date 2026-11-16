//! Cron scheduling backed by `tokio-cron-scheduler`.
//!
//! Semantics worth knowing before relying on it:
//! - Expressions are six fields (`sec min hour day-of-month month
//!   day-of-week`) and are evaluated in UTC; seconds are required.
//! - The underlying scheduler wakes roughly every 500 ms, so sub-second
//!   cron precision does not exist here.
//! - Each tick produces a fresh future from the factory. Ticks run to
//!   completion before the next one is attempted; a panicking tick is
//!   contained (error log + panic counter) and the schedule keeps ticking.
//! - A cron entry is a perpetual watcher registered in the global registry;
//!   `drain` and `cancel` remove it (and its job) instead of waiting for it.

use std::borrow::Cow;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use tokio_cron_scheduler::{Job, JobScheduler};

use crate::error::ScheduleError;
use crate::service::{TaskEntry, TaskKind};
use crate::task::{TaskId, TaskPriority};

impl crate::TaskService {
    /// Register a recurring job on a cron schedule.
    ///
    /// `factory` is called to produce the future for every tick; the future
    /// runs to completion before the next tick fires. Ticks run at the
    /// default [`TaskPriority::Normal`].
    ///
    /// Returns the task id; pass it to [`crate::TaskService::cancel`] to
    /// remove the job. After [`crate::TaskService::drain`] this fails with
    /// [`ScheduleError::Draining`].
    ///
    /// # Errors
    ///
    /// - [`ScheduleError::InvalidCron`] if `cron_expr` does not parse.
    /// - [`ScheduleError::Scheduler`] if the underlying scheduler rejects
    ///   the job or cannot be created.
    /// - [`ScheduleError::Draining`] if the service is draining.
    pub async fn cron<F, Fut>(
        &self,
        name: impl Into<Cow<'static, str>>,
        cron_expr: &str,
        factory: F,
    ) -> Result<TaskId, ScheduleError>
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let name = name.into().into_owned();
        if !self.inner.accepting.load(Ordering::Acquire) {
            return Err(ScheduleError::Draining);
        }
        let cron = self.inner.cron.clone();
        let scheduler = cron
            .get_or_try_init(|| async {
                let scheduler = JobScheduler::new()
                    .await
                    .map_err(|e| ScheduleError::Scheduler(e.to_string()))?;
                scheduler
                    .start()
                    .await
                    .map_err(|e| ScheduleError::Scheduler(e.to_string()))?;
                Ok::<_, ScheduleError>(tokio::sync::Mutex::new(scheduler))
            })
            .await?;

        let id = self.inner.alloc();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let shared = self.inner.clone();
        let factory = Arc::new(std::sync::Mutex::new(factory));
        let name_for_tick = name.clone();
        let job = Job::new_async(cron_expr, move |_uuid, _locked| {
            let factory = factory.clone();
            let cancel_rx = cancel_rx.clone();
            let shared = shared.clone();
            let name = name_for_tick.clone();
            Box::pin(async move {
                if *cancel_rx.borrow() {
                    return; // job removed or service draining
                }
                let tick = {
                    // a panicking factory must not abort the schedule loop
                    let mut guard = factory.lock().unwrap_or_else(|e| e.into_inner());
                    guard()
                };
                let join = shared.handle.spawn(tick);
                match join.await {
                    Ok(()) => {}
                    Err(e) if e.is_panic() => {
                        tracing::error!(task = %name, %id, "cron task tick panicked: {e}");
                        shared.record_panic();
                    }
                    Err(e) => {
                        tracing::warn!(task = %name, %id, "cron task tick aborted: {e}");
                    }
                }
            })
        })
        .map_err(|e| ScheduleError::InvalidCron(e.to_string()))?;

        let guid = scheduler
            .lock()
            .await
            .add(job)
            .await
            .map_err(|e| ScheduleError::Scheduler(e.to_string()))?;

        let accepted = self.inner.register(
            id,
            TaskEntry {
                name,
                priority: TaskPriority::Normal,
                started_at: Instant::now(),
                kind: TaskKind::Cron,
                cancel: cancel_tx,
                cron_uuid: Some(guid),
                scope: None,
            },
        );
        if !accepted {
            // raced with drain; the entry was finalized (which also removes
            // the cron job) inside register
            return Err(ScheduleError::Draining);
        }
        Ok(id)
    }
}
