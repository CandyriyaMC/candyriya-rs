//! Thin wrappers over the `metrics` facade for this crate.
//!
//! The name constants live in [`candyriya_metrics::names`] so the whole
//! proxy and the operator docs agree on one naming scheme. Emission is
//! atomic-cheap: no I/O, no locks, no allocation beyond what the facade
//! already does; with no recorder installed everything here is a no-op.
//!
//! Both IPC series carry the same two labels, both closed vocabularies:
//!
//! - `target` — the service-kind vocabulary (`ServiceKind::as_str`), never
//!   a unit name, never a pid.
//! - `outcome` — one of `ok`, `timeout`, `busy`, `not_running`, `panicked`,
//!   `type_mismatch`, `not_cappable`. The success label is exported from
//!   [`crate::ipc`] as [`crate::ipc::OUTCOME_OK`]; failures map via
//!   [`crate::CallError::outcome_label`].

use std::time::Duration;

use candyriya_metrics::names;

/// Announce the IPC metric set: describe the two series so scrapes carry
/// HELP text. Safe to call on every tree construction; descriptions are
/// idempotent, and with no recorder installed everything is a silent no-op.
pub(crate) fn describe() {
    metrics::describe_counter!(
        names::NAME_IPC_CALLS_TOTAL,
        "Unit-to-unit IPC calls by bounded target kind and outcome \
         (ok, timeout, busy, not_running, panicked, type_mismatch, not_cappable)."
    );
    metrics::describe_histogram!(
        names::NAME_IPC_CALL_DURATION_SECONDS,
        "Wall-clock seconds one unit-to-unit IPC call took, send to answer-or-failure."
    );
}

/// Record one call in `candyriya_ipc_calls_total`.
pub(crate) fn record_call(target: &'static str, outcome: &'static str) {
    metrics::counter!(
        names::NAME_IPC_CALLS_TOTAL,
        "target" => target,
        "outcome" => outcome,
    )
    .increment(1);
}

/// Observe one call's wall-clock duration in
/// `candyriya_ipc_call_duration_seconds`.
pub(crate) fn record_duration(target: &'static str, outcome: &'static str, duration: Duration) {
    metrics::histogram!(
        names::NAME_IPC_CALL_DURATION_SECONDS,
        "target" => target,
        "outcome" => outcome,
    )
    .record(duration.as_secs_f64());
}
