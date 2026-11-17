//! Fan-out of game events into the loaded plugins.
//!
//! [`ProxyEventDispatcher`] takes a [`ProxyEvent`], sends the serialized
//! [`EventEnvelope`] to every *loaded* plugin in load order, and collects
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
//!   event — the per-plugin outcomes are kept for debugging.
//! - **Cancellation wins.** If any answer has `cancelled = true`, the
//!   aggregate [`DispatchResult::cancelled`] is `true`, regardless of where
//!   in the order the cancel happened (or what earlier plugins said).
//!   `cancel_reason` carries the reason given by the *first* cancelling
//!   plugin in load order.
//! - **Timeouts do not cancel.** A plugin that does not answer within the
//!   timeout (default ten seconds) is logged as an error and recorded as
//!   [`Outcome::Timeout`]; it neither cancels nor blocks the others.
//! - **Garbage is not a crash.** A call failure or an outcome that does
//!   not parse as an [`OutcomeReport`] is recorded as [`Outcome::Failed`]
//!   and never aborts the dispatch.
//! - **Disabled and failed plugins are not dispatched to** and are not
//!   listed in the outcomes.
//!
//! The read guard on the plugin set is held for the whole dispatch;
//! [`crate::PluginReloader`] therefore waits for in-flight dispatches
//! before swapping a plugin instance. A dispatch holds it across wasm
//! calls — that is deliberate (a half-swapped dispatch would be worse) and
//! bounded by the per-plugin timeout.

use std::time::Duration;

use candyriya_plugin_api::{OutcomeReport, ProxyEvent};

use crate::error::PluginError;
use crate::instance::PluginInstance;
use crate::library_plugin::PluginState;
use crate::plugin_set::PluginSet;

/// The export the host calls on every loaded plugin for one event.
pub const ON_EVENT_EXPORT: &str = "on_event";

/// The default per-plugin timeout for one `on_event` call.
pub const DEFAULT_EVENT_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Fans events out into the loaded plugin set.
///
/// Cloning is not necessary; pass `&ProxyEventDispatcher` around. Build one
/// per plugin set (the proxy builds it at composition time).
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
                return DispatchResult {
                    cancelled: false,
                    cancel_reason: None,
                    per_plugin_outcomes: Vec::new(),
                };
            }
        };

        let (mut cancelled, mut cancel_reason) = (false, None);
        let mut per_plugin_outcomes = Vec::new();

        let guard = self.plugins.read();
        for (id, plugin) in guard.iter() {
            // disabled and failed plugins are not dispatched to and do
            // not appear in the outcomes (see the module docs)
            if plugin.state() != &PluginState::Loaded {
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

    match instance.call_with_timeout(ON_EVENT_EXPORT, payload, timeout) {
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
    }
}
