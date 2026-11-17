//! Prometheus exposition for the candyriya proxy.
//!
//! Metrics are recorded through the [`metrics`] facade: any crate in the
//! workspace emits with [`metrics::counter!`], [`metrics::gauge!`] and
//! [`metrics::histogram!`] and stays completely ignorant of this crate. This
//! crate owns two things:
//!
//! - [`install`], the one-time process-wide setup: builds the Prometheus
//!   recorder, points the facade at it, and optionally starts the embedded
//!   HTTP listener that Prometheus scrapes.
//! - [`names`], the shared metric name constants every crate uses, so the
//!   whole proxy agrees on one naming scheme.
//!
//! # Install semantics
//!
//! ```no_run
//! use std::net::SocketAddr;
//!
//! // Prometheus disabled: no recorder is installed, the facade macros become
//! // no-ops, and nothing is bound. This is cheap and always succeeds, so the
//! // composition root can call it unconditionally.
//! let handle = candyriya_metrics::install(None).expect("disabled install cannot fail");
//!
//! // Prometheus enabled: recorder installed, HTTP listener bound to `addr`.
//! // The bind happens synchronously inside `install`, so a busy port is
//! // reported as an `Err` right away.
//! let addr = SocketAddr::from(([0, 0, 0, 0], 9124));
//! match candyriya_metrics::install(Some(addr)) {
//!     Ok(handle) => {
//!         let _ = handle;
//!     }
//!     Err(candyriya_metrics::MetricsError::AlreadyInstalled) => {
//!         // somebody else got here first; that is fine, keep running
//!     }
//!     Err(e) => panic!("metrics endpoint failed: {e}"),
//! }
//! ```
//!
//! The one-shot rule applies to the *enabled* install only:
//!
//! - `install(None)` performs no global mutation whatsoever. It never fails,
//!   may be called any number of times, and does not consume the one-shot
//!   slot. When no recorder is installed, the facade macros compile to no-ops
//!   and never panic.
//! - `install(Some(addr))` is one-shot per process. The first call wins; every
//!   subsequent call returns [`MetricsError::AlreadyInstalled`] without binding
//!   or spawning anything. If the installation fails after the slot was
//!   claimed (address busy, runtime could not start), the slot is released so
//!   a retry is possible. If another recorder was installed by someone else
//!   before this call, the call also returns [`MetricsError::AlreadyInstalled`]
//!   and the slot stays claimed — installing can never succeed afterwards.
//!
//! # The exporter thread
//!
//! With `http-listener` enabled, the exporter binds a std `TcpListener`
//! synchronously during [`install`] and serves on a background thread with
//! its own single-threaded runtime. It is not stop-able through this crate's
//! API: the listener keeps serving until the process exits. Pull-based
//! scraping needs no flush and no shutdown hook, so that is by design, not a
//! missing feature.
//!
//! The HTTP listener responds on **every** path, so `GET /metrics` works as
//! expected; `/health` is a built-in special case (always `200 OK`).
//!
//! # Labels, buckets and naming
//!
//! - One global label `service="candyriya"` is attached to every metric.
//! - Counters are named with the `_total` suffix inside the constant in
//!   [`names`]; recommended naming is deliberately NOT enabled, so the
//!   exported name is exactly the constant's value.
//! - Histograms are rendered as true Prometheus histograms (not summaries)
//!   with [`DEFAULT_HISTOGRAM_BUCKETS`] as bounds. There are exactly two
//!   histograms — login duration and per-plugin call duration — everything
//!   else hot-path is a counter or a gauge.
//!
//! # Metric selection
//!
//! Hot paths (per packet, per byte) get counters and gauges only. The two
//! histograms exist for login duration and per-plugin call duration; even
//! those could be swapped for a pair of counters later if the buckets prove
//! contentious.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::sync::atomic::{AtomicBool, Ordering};

pub use metrics_exporter_prometheus::BuildError;

pub mod names;

/// Bucket bounds for histograms, in seconds.
///
/// This is the Prometheus default ladder, chosen because it brackets the
/// range we actually care about: sub-100ms handshake-fast paths at the low
/// end, and multi-second online-mode logins (sessionserver roundtrips) at the
/// high end.
///
/// Every histogram in the process renders with these bounds; per-metric
/// bucket tuning can be added later through `PrometheusBuilder` if a series
/// proves it needs it.
pub const DEFAULT_HISTOGRAM_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// Claims the one-shot slot for the enabled install.
///
/// `install(None)` never touches this, so disabled installs interleave freely
/// with an enabled install performed later in the same process.
static ENABLED_INSTALL_SLOT: AtomicBool = AtomicBool::new(false);

/// Errors returned by [`install`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MetricsError {
    /// Another recorder was already installed in this process, either by a
    /// previous enabled `install` call from this crate or by someone else's
    /// recorder. Retrying can never succeed in that situation.
    ///
    /// Note: if the failure happened because an external recorder was already
    /// installed, a stray exporter thread bound to the requested address may
    /// remain alive until process exit — a limitation of the underlying
    /// installer's spawn-then-install ordering.
    #[error("metrics recorder already installed in this process")]
    AlreadyInstalled,

    /// The exporter could not be built or the listener could not be bound.
    ///
    /// When this happens the one-shot slot is released, so the caller may fix
    /// the address and try again.
    #[error("failed to install the Prometheus exporter: {0}")]
    Install(#[from] BuildError),
}

/// Installs the metrics stack.
///
/// See the crate-level docs for the exact semantics. The short version:
///
/// - `None`: metrics disabled. Nothing bound, no recorder installed, facade
///   macros are no-ops. Always `Ok`.
/// - `Some(addr)`: recorder installed globally, Prometheus scrape endpoint
///   listening on `addr`. One-shot per process; failures release the slot.
///
/// Returns a [`MetricsHandle`] that can shut the installation down
/// explicitly.
pub fn install(address: Option<std::net::SocketAddr>) -> Result<MetricsHandle, MetricsError> {
    match address {
        None => {
            tracing::debug!("prometheus metrics disabled; facade macros are no-ops");
            Ok(MetricsHandle { enabled: false })
        }
        Some(addr) => install_prometheus(addr),
    }
}

fn install_prometheus(addr: std::net::SocketAddr) -> Result<MetricsHandle, MetricsError> {
    if ENABLED_INSTALL_SLOT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(MetricsError::AlreadyInstalled);
    }

    let result = build_exporter(addr).map(|()| MetricsHandle { enabled: true });
    match result {
        Ok(handle) => {
            tracing::info!(%addr, "prometheus metrics endpoint listening");
            Ok(handle)
        }
        Err(MetricsError::AlreadyInstalled) => {
            // the global recorder was set by someone else behind our back;
            // the slot stays claimed since install can never succeed now
            Err(MetricsError::AlreadyInstalled)
        }
        Err(e) => {
            ENABLED_INSTALL_SLOT.store(false, Ordering::Release);
            Err(e)
        }
    }
}

fn build_exporter(addr: std::net::SocketAddr) -> Result<(), MetricsError> {
    let builder = metrics_exporter_prometheus::PrometheusBuilder::new()
        .with_http_listener(addr)
        .add_global_label("service", "candyriya");

    let builder = match builder.set_buckets(DEFAULT_HISTOGRAM_BUCKETS) {
        Ok(builder) => builder,
        Err(e) => return Err(MetricsError::Install(e)),
    };

    match builder.install() {
        Ok(()) => Ok(()),
        Err(BuildError::FailedToSetGlobalRecorder(_)) => Err(MetricsError::AlreadyInstalled),
        Err(e) => Err(MetricsError::Install(e)),
    }
}

/// Guard for the metrics installation.
///
/// Returned by [`install`]. Cheap to keep around; the composition root
/// usually stores it for the lifetime of the process.
///
/// Dropping the handle performs no flush: the scrape endpoint is pull-based,
/// so nothing is buffered at this layer and there is nothing to flush at
/// shutdown. Dropping simply ends this crate's bookkeeping. The exporter's
/// background listener thread keeps serving until process exit, which is
/// intentional.
#[derive(Debug)]
pub struct MetricsHandle {
    enabled: bool,
}

impl MetricsHandle {
    /// Returns `true` if this installation enabled the Prometheus exporter.
    ///
    /// Useful for the composition root to log whether the scrape endpoint is
    /// live, or to skip wiring that only makes sense with metrics on.
    pub fn is_prometheus_enabled(&self) -> bool {
        self.enabled
    }

    /// Consumes the handle.
    ///
    /// Identical to dropping it: there is no fallible shutdown work to do.
    /// In pull mode Prometheus asks for the current state on every scrape, so
    /// no final flush is required.
    pub fn shutdown(self) {
        tracing::info!(
            prometheus = self.enabled,
            "metrics shutdown requested; any exporter keeps serving until process exit"
        );
        // dropping `self` here logs one more line at debug level via Drop
    }
}

impl Drop for MetricsHandle {
    fn drop(&mut self) {
        // nothing to flush in pull mode; the listener thread (if any) serves
        // until process exit. this is a guard, not a resource.
        tracing::debug!(prometheus = self.enabled, "metrics handle dropped");
    }
}
