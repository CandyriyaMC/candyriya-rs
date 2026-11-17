//! Fan-out of game events into the loaded plugins.
//!
//! [`ProxyEventDispatcher`] takes a [`ProxyEvent`], sends the serialized
//! [`EventEnvelope`](candyriya_plugin_api::EventEnvelope) to every *loaded* plugin in load order, and collects
//! what came back. A plugin is expected to export `on_event`; the payload
//! crosses the ABI as one flat JSON document and the answer is read back
//! as an [`OutcomeReport`] (`{"cancelled":true,"cancel_reason":"..."}`).
//!
//! (extism 1.x copies the guest's output out of wasm memory when the
//! export returns; there is no separate `take_output` step — reading the
//! bytes back is what [`crate::PluginInstance::call`] does.)
//!
//! # Semantics
//!
//! - **Order.** Plugins are asked in the order the loader produced them.
//!   Every loaded plugin is asked, even after one of them cancelled the
//!   event — the per-plugin outcomes are kept for debugging. A plugin
//!   whose `subscribers` allowlist (see the `candyriya:subscribeToEvents`
//!   host function) does not contain the event kind is *not asked* and
//!   does not appear in the outcomes; an empty allowlist means every kind.
//! - **Cancellation wins.** If any answer has `cancelled = true`, the
//!   aggregate [`DispatchResult::cancelled`] is `true`, regardless of where
//!   in the order the cancel happened (or what earlier plugins said).
//!   `cancel_reason` carries the reason given by the *first* cancelling
//!   plugin in load order. A cancellation recorded by a plugin through the
//!   `candyriya:cancelEvent` host function is ORed into its outcome: an
//!   explicit `cancel: true` wins even if the export itself answers
//!   "allowed", and beats a timeout or an undecodable answer.
//! - **Timeouts do not cancel.** A plugin that does not answer within the
//!   timeout (default ten seconds) is logged as an error and recorded as
//!   [`Outcome::Timeout`]; it neither cancels nor blocks the others —
//!   unless the plugin recorded a cancellation via `cancelEvent` before
//!   the timeout hit.
//! - **Garbage is not a crash.** A call failure or an outcome that does
//!   not parse as an [`OutcomeReport`] is recorded as [`Outcome::Failed`]
//!   and never aborts the dispatch.
//! - **Disabled and failed plugins are not dispatched to** and are not
//!   listed in the outcomes.
//! - **`callApi` routing.** [`ProxyEventDispatcher::call_api`] is the only
//!   path for one plugin to reach another plugin's `[api].exports`; host
//!   functions use it instead of touching the plugin set themselves.
//!
//! The aggregate types — [`Outcome`] and [`DispatchResult`] — and the
//! [`EventDispatcher`](candyriya_plugin_api::EventDispatcher) handle the
//! proxy stores are defined in `candyriya-plugin-api` and re-exported from
//! this crate (`candyriya_plugin::{Outcome, DispatchResult}`), so the
//! proxy can fire events without depending on the wasm runtime.
//!
//! The read guard on the plugin set is held for the whole dispatch;
//! [`crate::PluginReloader`] therefore waits for in-flight dispatches
//! before swapping a plugin instance. A dispatch holds it across wasm
//! calls — that is deliberate (a half-swapped dispatch would be worse) and
//! bounded by the per-plugin timeout.

use std::time::Duration;

pub use candyriya_plugin_api::{DispatchResult, Outcome};
use candyriya_plugin_api::{EventDispatcher, OutcomeReport, ProxyEvent};

use crate::error::PluginError;
use crate::instance::PluginInstance;
use crate::library_plugin::PluginState;
use crate::plugin_set::PluginSet;

/// The export the host calls on every loaded plugin for one event.
pub const ON_EVENT_EXPORT: &str = "on_event";

/// The default per-plugin timeout for one `on_event` call.
pub const DEFAULT_EVENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors produced by [`ProxyEventDispatcher::call_api`].
///
/// The `Display` strings are what plugin authors see over the wire in
/// `{"ok": false, "error": "..."}`; `NotLoaded` deliberately does not
/// distinguish "absent" from "disabled"/"failed" — callers get the same
/// answer either way.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallApiError {
    /// The target is missing from the plugin set or not currently loaded.
    #[error("plugin not loaded")]
    NotLoaded,
    /// The target does not list the requested export in `[api].exports`.
    #[error("plugin `{target}` does not export `{export}`")]
    NotExported {
        /// The target plugin's id.
        target: String,
        /// The export the caller asked for.
        export: String,
    },
    /// The call into the target failed or timed out.
    #[error("{0}")]
    Call(String),
}

/// Fans events out into the loaded plugin set.
///
/// Cloning is not necessary; pass `&ProxyEventDispatcher` around. Build one
/// per plugin set (the proxy builds it at composition time). Host functions
/// that route between plugins go through [`ProxyEventDispatcher::call_api`]
/// rather than touching the set themselves.
#[derive(Debug)]
pub struct ProxyEventDispatcher {
    plugins: PluginSet,
    timeout: Duration,
}

impl ProxyEventDispatcher {
    /// A dispatcher with the default ten-second per-plugin timeout.
    pub fn new(plugins: PluginSet) -> Self {
        Self::with_timeout(plugins, DEFAULT_EVENT_TIMEOUT)
    }

    /// A dispatcher with an explicit per-plugin timeout.
    pub fn with_timeout(plugins: PluginSet, timeout: Duration) -> Self {
        Self { plugins, timeout }
    }

    /// The timeout each plugin gets for one `on_event` call.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The plugin set this dispatcher fans out into.
    pub fn plugins(&self) -> &PluginSet {
        &self.plugins
    }

    /// Call `fn_name` on plugin `target` on behalf of plugin `caller`.
    ///
    /// The function must be listed in the target's `[api].exports` and is
    /// the only thing the target may serve to other plugins. The caller's
    /// identity is installed on the target for the duration of the call,
    /// so the target's host functions see a meaningful caller via
    /// `caller_plugin_id`. Returns the target's raw output bytes.
    ///
    /// # Errors
    ///
    /// - [`CallApiError::NotLoaded`] — the target is missing from the set
    ///   or not in [`PluginState::Loaded`] state. The wire string is exactly
    ///   `plugin not loaded`.
    /// - [`CallApiError::NotExported`] — the target does not list
    ///   `fn_name` in its `[api].exports`.
    /// - [`CallApiError::Call`] — the call into the target failed or
    ///   timed out; the message is a human-readable diagnosis.
    pub fn call_api(
        &self,
        target: &str,
        fn_name: &str,
        args: &[u8],
        caller: Option<&str>,
    ) -> Result<Vec<u8>, CallApiError> {
        call_target(&self.plugins, self.timeout, target, fn_name, args, caller)
    }

    /// Dispatch `event` into every loaded plugin, in load order, and
    /// aggregate the answers.
    ///
    /// See the module docs for the full semantics. The event itself is
    /// not mutated; the aggregate cancellation state lives in the
    /// returned [`DispatchResult`]. Host code that wants the ambient
    /// accumulator flipped can call [`ProxyEvent::cancel`] with the
    /// result's reason.
    pub fn dispatch_cancellable(&self, event: &ProxyEvent) -> DispatchResult {
        // the envelope is the one shape the ABI commits to; the failure
        // path exists because "cannot fail" is not the same as "no
        // branch". if it ever does fail, no plugin sees the event and
        // the dispatch is a no-op by design.
        let payload = match serde_json::to_vec(event.envelope()) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::error!(%error, "failed to serialize the event envelope; dispatch skipped");
                return DispatchResult::default();
            }
        };

        let mut cancelled = false;
        let mut cancel_reason = None;
        let mut per_plugin_outcomes = Vec::new();
        let kind = event.envelope().kind.as_str();

        let guard = self.plugins.read();
        for (id, plugin) in guard.iter() {
            // disabled and failed plugins are not dispatched to and do
            // not appear in the outcomes (see the module docs)
            if plugin.state() != &PluginState::Loaded {
                continue;
            }
            // subscriber gate: an empty allowlist means every kind; a
            // non-empty one must carry this event's kind string. the
            // skipped plugin is not listed in the outcomes.
            if let Some(instance) = plugin.instance()
                && !instance.subscribes_to(kind)
            {
                continue;
            }
            let outcome = dispatch_one(id, plugin.instance(), &payload, self.timeout);
            if let Outcome::Cancelled {
                cancel_reason: ref plugin_reason,
            } = outcome
            {
                cancelled = true;
                // first cancelling plugin in load order names the reason
                if cancel_reason.is_none() {
                    cancel_reason = plugin_reason.clone();
                }
            }
            per_plugin_outcomes.push((id.clone(), outcome));
        }

        DispatchResult {
            cancelled,
            cancel_reason,
            per_plugin_outcomes,
        }
    }
}

/// Route `fn_name` into plugin `target` on behalf of plugin `caller`.
///
/// This is the path `candyriya:callApi` takes: look the target up in the
/// set, refuse disabled/failed/missing targets, gate on the target's
/// `[api].exports`, and invoke the export with the caller's identity set
/// so the target's host functions can authorize it. The target's previous
/// caller identity is restored afterwards.
fn call_target(
    plugins: &PluginSet,
    timeout: Duration,
    target: &str,
    fn_name: &str,
    args: &[u8],
    caller: Option<&str>,
) -> Result<Vec<u8>, CallApiError> {
    let guard = plugins.read();
    let Some(record) = guard.get(target) else {
        return Err(CallApiError::NotLoaded);
    };
    if record.state() != &PluginState::Loaded {
        return Err(CallApiError::NotLoaded);
    }
    let exports = &record.manifest().metadata().api.exports;
    if !exports.iter().any(|export| export.name == fn_name) {
        return Err(CallApiError::NotExported {
            target: target.to_owned(),
            export: fn_name.to_owned(),
        });
    }
    let Some(instance) = record.instance() else {
        return Err(CallApiError::NotLoaded);
    };
    let previous = instance.set_caller_plugin_id(caller.map(str::to_owned));
    // a cancellation left over from a previous call must not leak in
    let _ = instance.take_pending_outcome();
    let result = instance.call_with_timeout(fn_name, args, timeout);
    // mechanical outcome accounting; callApi has no dispatch-level merge
    crate::metrics::record_result(target, &result);
    // cancelEvent only makes sense for event dispatches; inside a callApi
    // the recorded cancellation is discarded
    let _ = instance.take_pending_outcome();
    instance.set_caller_plugin_id(previous);
    result.map_err(|error| CallApiError::Call(error.to_string()))
}

// the proxy stores this as an `Arc<dyn EventDispatcher>` and never names
// the runtime crate, so the contract is implemented once here
impl EventDispatcher for ProxyEventDispatcher {
    fn dispatch_event(&self, event: &ProxyEvent) -> DispatchResult {
        self.dispatch_cancellable(event)
    }
}

fn dispatch_one(
    id: &str,
    instance: Option<&PluginInstance>,
    payload: &[u8],
    timeout: Duration,
) -> Outcome {
    let Some(instance) = instance else {
        return Outcome::Failed {
            error: "plugin record has no instance attached".to_owned(),
        };
    };

    // the proxy is the caller for top-level dispatches; restore whatever
    // was there afterwards so nested callApi chains stay balanced
    let previous = instance.set_caller_plugin_id(None);
    // a cancellation left over from a previous call must not leak in
    let _ = instance.take_pending_outcome();
    let result = instance.call_with_timeout(ON_EVENT_EXPORT, payload, timeout);
    let pending = instance.take_pending_outcome();
    instance.set_caller_plugin_id(previous);

    // keep one bit of the raw result: fuel exhaustion is its own outcome
    // label even though the dispatcher folds it into `failed`. a busy
    // rejection must not land in calls_total at all (busy_total owns it).
    let fuel_exhausted = matches!(&result, Err(PluginError::FuelExhausted { .. }));
    let busy = matches!(&result, Err(PluginError::Busy { .. }));
    let base = match result {
        Ok(bytes) => match serde_json::from_slice::<OutcomeReport>(&bytes) {
            Ok(report) => {
                if report.cancelled {
                    Outcome::Cancelled {
                        cancel_reason: report.cancel_reason,
                    }
                } else {
                    Outcome::Allowed
                }
            }
            Err(error) => Outcome::Failed {
                error: format!("plugin `{id}` returned an undecodable outcome: {error}"),
            },
        },
        Err(PluginError::Timeout { timeout_ms, .. }) => {
            tracing::error!(
                plugin = %id,
                timeout_ms,
                "plugin did not answer the event dispatch within its timeout; treating it as not cancelling",
            );
            Outcome::Timeout
        }
        Err(error) => Outcome::Failed {
            error: error.to_string(),
        },
    };
    let outcome = merge_pending_cancellation(base, pending);
    // the merged outcome is the truthful one for on_event: a cancellation
    // recorded through cancelEvent (or in the export's own report) wins
    // over the mechanical result, so `cancelled` — and nothing else —
    // lands here for those calls. busy rejections counted nothing:
    // busy_total owns mailbox-full, calls_total stays silent.
    let metric_outcome = match &outcome {
        Outcome::Allowed => Some(crate::metrics::CallOutcome::Ok),
        Outcome::Cancelled { .. } => Some(crate::metrics::CallOutcome::Cancelled),
        Outcome::Timeout => Some(crate::metrics::CallOutcome::Timeout),
        Outcome::Failed { .. } if fuel_exhausted => {
            Some(crate::metrics::CallOutcome::FuelExhausted)
        }
        Outcome::Failed { .. } if busy => None,
        Outcome::Failed { .. } => Some(crate::metrics::CallOutcome::Failed),
    };
    if let Some(metric_outcome) = metric_outcome {
        crate::metrics::record_call(id, metric_outcome);
    }
    outcome
}

/// Fold a cancellation recorded through the `candyriya:cancelEvent` host
/// function into the outcome reported by the export itself.
///
/// An explicit `cancel: true` wins over everything — allowed, undecodable
/// output, a trap, even a timeout. A `cancel: false` records nothing.
fn merge_pending_cancellation(outcome: Outcome, pending: Option<OutcomeReport>) -> Outcome {
    match pending {
        None => outcome,
        Some(report) if !report.cancelled => outcome,
        Some(report) => match outcome {
            Outcome::Cancelled { cancel_reason } => Outcome::Cancelled {
                // the host-function cancellation is the fresher signal; keep
                // the export's reason only when it has none
                cancel_reason: report.cancel_reason.or(cancel_reason),
            },
            // allowed, undecodable output, a trap or a timeout — the
            // explicit host-function cancellation wins regardless
            _ => Outcome::Cancelled {
                cancel_reason: report.cancel_reason,
            },
        },
    }
}
