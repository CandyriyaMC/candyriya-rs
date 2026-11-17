//! The event dispatch contract shared by the host and the runtime.
//!
//! An event dispatch fans one [`ProxyEvent`] out into the loaded plugin
//! set and aggregates what came back. The *runtime* side of that lives in
//! `candyriya-plugin` (which re-exports the types defined here); the
//! *consumer* side — the proxy's lifecycle wiring — needs just two things
//! from it: the call that fans one event out, and the aggregate it returns.
//! Both are defined here so the proxy can depend on the contract without
//! linking the wasm runtime.
//!
//! - [`Outcome`] / [`DispatchResult`] — the aggregate a dispatch produces.
//!   Their shape is part of the ABI's documented contract (see
//!   `docs/plugins.md`); the runtime reports them, the proxy acts on them.
//! - [`EventDispatcher`] — the object-safe handle the proxy stores to
//!   reach the runtime's dispatcher. Implemented by
//!   `candyriya_plugin::ProxyEventDispatcher`; the proxy keeps an
//!   `Arc<dyn EventDispatcher>` and never names the runtime directly.

use crate::events::ProxyEvent;

/// What came back from one plugin for one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The plugin ran and did not cancel the event.
    Allowed,
    /// The plugin ran and cancelled the event, with an optional reason.
    Cancelled {
        /// The human-readable reason the plugin gave, if any.
        cancel_reason: Option<String>,
    },
    /// The plugin did not answer within the timeout. Treated as not
    /// cancelling; logged as an error.
    Timeout,
    /// The call failed or the answer was not a valid outcome report.
    /// The string is a human-readable diagnosis.
    Failed {
        /// What went wrong, for logs.
        error: String,
    },
}

/// The aggregate result of one event dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchResult {
    /// `true` when at least one plugin cancelled the event.
    pub cancelled: bool,
    /// The reason given by the first cancelling plugin in load order, if
    /// any. `None` when nothing cancelled or the cancelling plugin gave
    /// no reason.
    pub cancel_reason: Option<String>,
    /// One `(plugin_id, outcome)` pair per loaded plugin, in load order.
    pub per_plugin_outcomes: Vec<(String, Outcome)>,
}

impl DispatchResult {
    /// How many plugins answered with `allowed`.
    pub fn allowed_count(&self) -> usize {
        self.per_plugin_outcomes
            .iter()
            .filter(|(_, outcome)| matches!(outcome, Outcome::Allowed))
            .count()
    }

    /// The outcome reported by a specific plugin, if it was dispatched to.
    pub fn outcome_for(&self, plugin: &str) -> Option<&Outcome> {
        self.per_plugin_outcomes
            .iter()
            .find(|(id, _)| id == plugin)
            .map(|(_, outcome)| outcome)
    }
}

/// An aggregate that decided nothing: nothing cancelled and no plugin was
/// asked. Used for the "dispatch could not be built" path (a serialized
/// envelope that does not fit the ABI), never as a stand-in for a real
/// dispatch.
impl Default for DispatchResult {
    fn default() -> Self {
        Self {
            cancelled: false,
            cancel_reason: None,
            per_plugin_outcomes: Vec::new(),
        }
    }
}

/// The host-side handle the proxy uses to fan events into plugins.
///
/// Implemented by `candyriya_plugin::ProxyEventDispatcher`. The proxy
/// stores an `Arc<dyn EventDispatcher>` in its state so it can fire
/// lifecycle events without depending on the plugin runtime's crate,
/// the same way host functions resolve services from the registry.
pub trait EventDispatcher: Send + Sync {
    /// Fan `event` out into every loaded plugin, in load order, and
    /// aggregate the answers. See the runtime's dispatcher docs for the
    /// full cancellation and timeout semantics.
    fn dispatch_event(&self, event: &ProxyEvent) -> DispatchResult;
}
