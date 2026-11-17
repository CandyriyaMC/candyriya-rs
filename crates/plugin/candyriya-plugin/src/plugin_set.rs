//! The ordered set of discovered plugins the host runs.
//!
//! [`PluginSet`] is the shared, thread-safe handle the event dispatcher
//! reads from and the hot-reload machinery swaps records in and out of.
//! It preserves load order (the order the loader produced), which is also
//! the order events are dispatched in.
//!
//! All access goes through one `parking_lot::RwLock`. Readers (dispatch)
//! hold the read guard for as long as a plugin call takes; writers
//! (reload) wait for in-flight dispatches. There is no lock-free fast
//! path here on purpose: a dispatch that observes a half-swapped plugin
//! set would be much worse than one that waits a millisecond.

use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::library_plugin::{LibraryPlugin, PluginState};

/// The ordered set of plugins the host knows about.
///
/// Cloning is cheap (an `Arc` clone); every clone refers to the same
/// underlying set.
#[derive(Clone, Default)]
pub struct PluginSet {
    inner: Arc<RwLock<IndexMap<String, LibraryPlugin>>>,
}

impl PluginSet {
    /// Create an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a set from loader output.
    ///
    /// Insertion order is the dispatch order. If the same id shows up
    /// twice the first record wins (the loader already rejects duplicates;
    /// this rerun is a bug detector, not a policy).
    pub fn from_vec(plugins: Vec<LibraryPlugin>) -> Self {
        let mut map = IndexMap::with_capacity(plugins.len());
        for plugin in plugins {
            map.entry(plugin.id().to_owned()).or_insert(plugin);
        }
        Self {
            inner: Arc::new(RwLock::new(map)),
        }
    }

    /// Number of records in the set, any state.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// Whether the set holds no records at all.
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// The plugin ids, in load (dispatch) order.
    pub fn ids(&self) -> Vec<String> {
        self.inner.read().keys().cloned().collect()
    }

    /// Whether a record for `id` exists (any state).
    pub fn contains(&self, id: &str) -> bool {
        self.inner.read().contains_key(id)
    }

    /// The lifecycle state of `id`, if present.
    pub fn state(&self, id: &str) -> Option<PluginState> {
        self.inner.read().get(id).map(|p| p.state().clone())
    }

    pub(crate) fn read(&self) -> RwLockReadGuard<'_, IndexMap<String, LibraryPlugin>> {
        self.inner.read()
    }

    pub(crate) fn write(&self) -> RwLockWriteGuard<'_, IndexMap<String, LibraryPlugin>> {
        self.inner.write()
    }
}

impl std::fmt::Debug for PluginSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginSet")
            .field("ids", &self.ids())
            .finish()
    }
}
