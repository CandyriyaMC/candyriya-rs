//! Per-plugin metrics emitted through the `metrics` facade.
//!
//! Everything in here is a thin, crate-local wrapper over the facade; the
//! metric name constants live in [`candyriya_metrics::names`] so the
//! composition root and the operator docs agree on one naming scheme. All
//! series carry a `plugin` label with the plugin id from the manifest —
//! the id charset is bounded (`[a-z0-9_.-]{1,64}`) so label cardinality is
//! bounded by the number of plugins the operator loads.
//!
//! # What is counted where
//!
//! The call path is split deliberately:
//!
//! - [`PluginInstance::call_with_timeout`](crate::PluginInstance::call_with_timeout)
//!   records the *mechanics*: the active-call gauge (an in-flight call exists
//!   from the moment it is sent until the answer or timeout comes back), the
//!   duration histogram (wall-clock seconds send-to-answer), a busy counter
//!   bump on mailbox-full rejections, and worker-thread wasm time on the
//!   plugin thread itself. It does NOT touch `candyriya_plugin_calls_total` —
//!   see below for why.
//! - The *call sites* record `candyriya_plugin_calls_total`, exactly once per
//!   invocation, because only they know which outcome label is right:
//!   [`crate::PluginInstance::call`] and `ProxyEventDispatcher::call_api`
//!   use the mechanical mapping ([`outcome_from_result`]), while
//!   `dispatch_one` folds the parsed `OutcomeReport` and the pending
//!   `cancelEvent` cancellation into `cancelled`. Splitting it this way
//!   keeps every invocation on exactly one series and keeps `cancelled`
//!   meaning "the plugin cancelled the event", not "the wasm returned".
//!
//! Busy rejections (`plugin is busy (mailbox full)`) never reach the
//! mailbox, so there is no call to time and no call outcome to count; they
//! land in `candyriya_plugin_busy_total` only. Pre-flight rejections
//! (input too large, rate limited) are counted as `failed` in
//! `candyriya_plugin_calls_total`.
//!
//! # Lifecycle
//!
//! Per-plugin series come into existence on first emission; the `metrics`
//! facade cannot deregister series, so on unload they are kept, and
//! [`NAME_PLUGIN_LOADED`](candyriya_metrics::names::NAME_PLUGIN_LOADED) is
//! the load/unload signal operators alert on: it counts live wasm
//! instances — `1` loaded, `0` after it is dropped (unload, reload swap,
//! failed `plugin_main` check), briefly `2` while a hot-reload replacement
//! and the outgoing instance coexist. The gauge is a plain inc/dec pair on
//! the instance's birth and death, which keeps it correct across swaps (the
//! replacement increments before the outgoing instance decrements) without
//! a setter that could clobber a concurrent stage.
//!
//! # Thread time
//!
//! `candyriya_plugin_thread_time_seconds_total` is cumulative wall-clock
//! seconds the plugin worker thread spent inside wasm calls. The facade has
//! no float counters, so it rides on a monotonically increasing gauge;
//! PromQL holds `rate()`/`increase()` over it just fine. True per-thread CPU
//! time is out of reach in safe Rust, and a call that never returns is not
//! accounted until the runtime interrupts it (fuel exhaustion or the
//! manifest timeout) — the same envelope the sandbox already enforces.

use std::time::Duration;

use candyriya_metrics::names;

use crate::error::PluginError;

/// The bounded `outcome` label set for `candyriya_plugin_calls_total`.
///
/// `cancelled` is a dispatch-level signal (the plugin cancelled the event);
/// the instance call path cannot produce it — it only sees bytes or a
/// mechanical error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallOutcome {
    /// The call ran and handed back bytes.
    Ok,
    /// The caller's deadline passed before the plugin answered.
    Timeout,
    /// The call was rejected before it ran (input too large, rate limited),
    /// the plugin errored, or the answer was unusable.
    Failed,
    /// The wasm fuel budget ran dry mid-call.
    FuelExhausted,
    /// The event dispatcher saw this plugin cancel the event.
    Cancelled,
}

impl CallOutcome {
    fn as_label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Timeout => "timeout",
            Self::Failed => "failed",
            Self::FuelExhausted => "fuel_exhausted",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Mechanical `outcome` mapping for results that carry no dispatch
/// semantics (`call`, `callApi`).
///
/// `None` means "not really a call": the mailbox was full, so the plugin
/// never saw anything; those rejections are counted in
/// `candyriya_plugin_busy_total` instead.
pub(crate) fn outcome_from_result(result: &Result<Vec<u8>, PluginError>) -> Option<CallOutcome> {
    match result {
        Ok(_) => Some(CallOutcome::Ok),
        Err(PluginError::Timeout { .. }) => Some(CallOutcome::Timeout),
        Err(PluginError::FuelExhausted { .. }) => Some(CallOutcome::FuelExhausted),
        Err(PluginError::Busy { .. }) => None,
        Err(_) => Some(CallOutcome::Failed),
    }
}

/// Record `candyriya_plugin_calls_total` from a mechanical result.
///
/// A no-op for mailbox-full rejections ([`outcome_from_result`] yields
/// `None`); the busy counter already covered them.
pub(crate) fn record_result(plugin: &str, result: &Result<Vec<u8>, PluginError>) {
    if let Some(outcome) = outcome_from_result(result) {
        record_call(plugin, outcome);
    }
}

/// Record one call in `candyriya_plugin_calls_total` with an explicit
/// outcome label.
pub(crate) fn record_call(plugin: &str, outcome: CallOutcome) {
    metrics::counter!(
        names::NAME_PLUGIN_CALLS_TOTAL,
        "plugin" => plugin.to_owned(),
        "outcome" => outcome.as_label(),
    )
    .increment(1);
}

/// Observe the wall-clock span of one plugin call in
/// `candyriya_plugin_call_duration_seconds`.
pub(crate) fn observe_call_duration(plugin: &str, duration: Duration) {
    metrics::histogram!(
        names::NAME_PLUGIN_CALL_DURATION_SECONDS,
        "plugin" => plugin.to_owned(),
    )
    .record(duration.as_secs_f64());
}

/// Add `duration` of wasm execution time to the plugin's cumulative
/// `candyriya_plugin_thread_time_seconds_total` gauge.
pub(crate) fn record_thread_time(plugin: &str, duration: Duration) {
    metrics::gauge!(
        names::NAME_PLUGIN_THREAD_TIME_SECONDS_TOTAL,
        "plugin" => plugin.to_owned(),
    )
    .increment(duration.as_secs_f64());
}

/// A call the plugin is currently working on (or on which a caller is
/// waiting). Increments `candyriya_plugin_active_calls`; the guard
/// decrements it on drop, so every exit path of the call — including
/// timeouts and dropped receivers — is accounted.
#[must_use = "the gauge is decremented when the guard is dropped"]
pub(crate) struct ActiveCallGuard {
    plugin: String,
}

impl Drop for ActiveCallGuard {
    fn drop(&mut self) {
        metrics::gauge!(
            names::NAME_PLUGIN_ACTIVE_CALLS,
            "plugin" => self.plugin.clone(),
        )
        .decrement(1.0);
    }
}

/// Increment `candyriya_plugin_active_calls` and return the guard that
/// decrements on drop.
pub(crate) fn active_call(plugin: &str) -> ActiveCallGuard {
    metrics::gauge!(
        names::NAME_PLUGIN_ACTIVE_CALLS,
        "plugin" => plugin.to_owned(),
    )
    .increment(1.0);
    ActiveCallGuard {
        plugin: plugin.to_owned(),
    }
}

/// Bump `candyriya_plugin_busy_total`: the mailbox was full, the plugin is
/// busy with earlier work, the call was refused before it ran.
pub(crate) fn record_busy(plugin: &str) {
    metrics::counter!(
        names::NAME_PLUGIN_BUSY_TOTAL,
        "plugin" => plugin.to_owned(),
    )
    .increment(1);
}

/// Announce a plugin coming up: describe the metric set (so the scrape
/// output carries HELP text) and bump `candyriya_plugin_loaded`.
pub(crate) fn plugin_loaded(plugin: &str) {
    describe_metrics();
    metrics::gauge!(
        names::NAME_PLUGIN_LOADED,
        "plugin" => plugin.to_owned(),
    )
    .increment(1.0);
}

/// Announce a plugin's instance going away: drop the
/// `candyriya_plugin_loaded` gauge back down. Series are kept; see the
/// module docs.
pub(crate) fn plugin_unloaded(plugin: &str) {
    metrics::gauge!(
        names::NAME_PLUGIN_LOADED,
        "plugin" => plugin.to_owned(),
    )
    .decrement(1.0);
}

/// Register HELP text for the metric family. Safe to call on every plugin
/// load: the descriptions are identical each time and the recorders that
/// care (prometheus) just re-store the same values. No recorder installed
/// (metrics disabled) means every describe call is a silent no-op.
fn describe_metrics() {
    metrics::describe_counter!(
        names::NAME_PLUGIN_CALLS_TOTAL,
        "Plugin calls by outcome label (ok, timeout, failed, fuel_exhausted, cancelled)."
    );
    metrics::describe_histogram!(
        names::NAME_PLUGIN_CALL_DURATION_SECONDS,
        "Wall-clock seconds one plugin call took, from send to answer-or-timeout."
    );
    metrics::describe_gauge!(
        names::NAME_PLUGIN_THREAD_TIME_SECONDS_TOTAL,
        "Cumulative wall-clock seconds the plugin worker thread spent inside wasm calls."
    );
    metrics::describe_gauge!(
        names::NAME_PLUGIN_ACTIVE_CALLS,
        "Plugin calls currently sent or being awaited."
    );
    metrics::describe_counter!(
        names::NAME_PLUGIN_BUSY_TOTAL,
        "Calls rejected because the plugin's mailbox was full."
    );
    metrics::describe_gauge!(
        names::NAME_PLUGIN_LOADED,
        "Number of live wasm instances for the plugin (normally 1; 2 briefly during a hot-reload swap)."
    );
}
