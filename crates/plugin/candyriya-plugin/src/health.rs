//! Per-plugin health cells: the bridge from the wasm worker to the
//! supervision tree.
//!
//! Today a wasm trap, an out-of-fuel abort or a host-side panic is
//! *contained* (the caller gets a `PluginError`, the worker carries on) —
//! but nothing about that outcome is visible as a state anywhere, so no
//! operator and no supervisor can ever say "this plugin is broken". The
//! cells here are the missing signal: one sticky flag per plugin id,
//! raised by the worker the moment one of the unambiguously abnormal
//! outcomes happens, and parked on by the `plugin-<id>` supervision unit
//! that reports the failure into the tree.
//!
//! # What raises a cell (and what does not)
//!
//! Raised, firsthand at the worker boundary in `worker_loop`:
//!
//! - a host-side panic caught by the `catch_unwind` around the wasm call;
//! - fuel exhaustion (the extism error message carries
//!   `"ran out of fuel"`) — the guest burned its whole instruction
//!   budget, i.e. it ran away, not merely erred;
//! - the manifest epoch interruption — extism 1.41 surfaces this as
//!   exactly `"timeout"` (`Error::msg("timeout")`, verified against the
//!   pinned extism source), i.e. a hung call the sandbox had to kill.
//!
//! Deliberately NOT raised: an ordinary `CallFailed` (a guest error is a
//! guest error — the dispatcher treats it as non-fatal by design),
//! `Busy`/`RateLimited` rejections (the mailbox, not the plugin), and a
//! caller-side `recv_timeout` (the plugin merely missed this caller's
//! deadline; the 30s manifest timer is the timeout that convicts).
//!
//! # Lifecycle
//!
//! Cells are sticky across instances on purpose (the cell belongs to the
//! plugin id, not to any one wasm instance) — with one deliberate reset:
//! building an instance for the id (initial load, `/candyriya reload`,
//! or the supervision-driven restage) hands it a clean slate. A restage
//! is the moment the operator (or the restart policy) explicitly asked
//! for a fresh plugin; carrying the old conviction into the fresh
//! instance would restart-loop a healthy replacement. The reset only
//! fires for cells that are actually flagged, so a clean build never
//! fabricates a wake-up for a parked body.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::watch;

/// One per-plugin health cell, shared between the worker that raises it
/// and the supervision unit that parks on it.
pub struct PluginHealth {
    broken: watch::Sender<bool>,
    reason: Mutex<Option<String>>,
}

impl PluginHealth {
    fn new() -> Self {
        let (broken, _rx) = watch::channel(false);
        Self {
            broken,
            reason: Mutex::new(None),
        }
    }

    /// Subscribe to the cell's state. Cells start `false`; the flag
    /// flips to `true` at most once per instance build.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.broken.subscribe()
    }

    /// Whether the plugin is flagged broken right now.
    pub fn is_broken(&self) -> bool {
        *self.broken.borrow()
    }

    /// The first reason the cell was flagged, if any. Diagnostics only;
    /// cleared on the next instance build for the id.
    pub fn reason(&self) -> Option<String> {
        self.reason.lock().clone()
    }

    /// Flag the plugin broken. Sticky until the next instance build for
    /// the id; the first reason wins (least surprise for `ps`).
    pub(crate) fn mark_broken(&self, reason: impl Into<String>) {
        {
            let mut slot = self.reason.lock();
            if slot.is_none() {
                *slot = Some(reason.into());
            }
        }
        if *self.broken.borrow() {
            return;
        }
        // send_replace so a flag raised with zero parked bodies (e.g. a
        // plugin flagged before its unit is spawned) is still recorded
        let _ = self.broken.send_replace(true);
    }

    /// Clear the flag if it was set (an instance for the id is being
    /// built, i.e. a fresh start was requested). No-op otherwise — no
    /// fabricated `changed()` for a parked body on an already-clean cell.
    fn reset(&self) {
        if !*self.broken.borrow() {
            return;
        }
        *self.reason.lock() = None;
        let _ = self.broken.send_replace(false);
    }
}

/// The id-keyed registry of health cells, shared by the loader, the
/// reloader and the supervision units.
#[derive(Clone, Default)]
pub struct PluginHealthSet {
    inner: Arc<Mutex<HashMap<String, Arc<PluginHealth>>>>,
}

impl PluginHealthSet {
    /// Create an empty registry. Cells are created lazily per plugin id.
    pub fn new() -> Self {
        Self::default()
    }

    /// The cell for `id`, creating it on first use (no reset).
    pub fn cell(&self, id: &str) -> Arc<PluginHealth> {
        let mut map = self.inner.lock();
        Arc::clone(
            map.entry(id.to_owned())
                .or_insert_with(|| Arc::new(PluginHealth::new())),
        )
    }

    /// The cell for `id` for a *fresh* instance build: get-or-create,
    /// then clear any conviction from a previous instance. Called by the
    /// loader and the reloader every time they build an instance.
    pub(crate) fn fresh_cell(&self, id: &str) -> Arc<PluginHealth> {
        let cell = self.cell(id);
        cell.reset();
        cell
    }
}

impl std::fmt::Debug for PluginHealthSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHealthSet")
            .field("ids", &self.inner.lock().keys().collect::<Vec<_>>())
            .finish()
    }
}
