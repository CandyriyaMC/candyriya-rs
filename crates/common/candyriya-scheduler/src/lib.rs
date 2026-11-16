//! Central task service for the proxy.
//!
//! Every background, delayed, repeating or cron-scheduled piece of work in
//! the system goes through [`TaskService`]; nothing runs ad hoc outside of
//! it. The service owns execution contexts end to end:
//!
//! - **async tasks** run on the runtime passed to [`TaskServiceBuilder`] —
//!   the service never creates its own runtime.
//! - **blocking work** runs on a fixed-size `rayon` pool sized at build time.
//! - **plugin (scoped) work** runs through [`ScopedTaskService`] with hard
//!   concurrency/queue quotas, a per-task timeout, low priority and its own
//!   counters.
//!
//! # Quick start
//!
//! ```no_run
//! use candyriya_scheduler::TaskService;
//! use std::time::Duration;
//!
//! # fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let rt = tokio::runtime::Builder::new_multi_thread()
//!     .worker_threads(2)
//!     .build()?;
//! let service = TaskService::builder()
//!     .handle(rt.handle().clone())
//!     .blocking_threads(4)
//!     .build()?;
//!
//! service.spawn_named("heartbeat", async {});
//!
//! let report = rt.block_on(service.drain(Duration::from_secs(5)));
//! println!("completed: {}", report.completed);
//! # Ok(()) }
//! ```
//!
//! # Guarantees
//!
//! - **Containment** — a panicking task is logged and counted
//!   ([`TaskStats::panicked`]); everything else keeps running.
//! - **Cancellation** — cooperative, via [`TaskService::cancel`]; async
//!   tasks stop at the next await point with destructors intact.
//! - **Graceful drain** — [`TaskService::drain`] stops admission, cancels
//!   perpetual watchers, waits for finite work and reports the outcome.
//! - **No `unsafe`** — this crate is forbidden from using it.
//!
//! # Guarantees against the API
//!
//! - Public items are documented on the item; nothing in the public API
//!   panics as part of normal control flow.
//! - Scope (plugin) tasks are always [`TaskPriority::Low`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod cron;
mod error;
mod scope;
mod service;
mod stats;
mod task;

pub use error::{BuildError, ScheduleError, ScopeError};
pub use scope::{ScopeLimits, ScopeStats, ScopedTaskService};
pub use service::{TaskService, TaskServiceBuilder};
pub use stats::{ShutdownReport, TaskStats};
pub use task::{TaskId, TaskPriority};
