//! The in-memory `id -> record` registry.
//!
//! This is the single owner of plugin records inside the controller. It
//! enforces one discipline above all: **no state moves except through the
//! allow-list in [`crate::ALLOWED_TRANSITIONS`]**. Everything else —
//! insertion, lookup, snapshotting — is ordinary bookkeeping in
//! discovery order (libraries first, then plugins, alphabetical), which
//! is also the enable order the machine will consume later.
//!
//! Interior mutability is a [`std::sync::RwLock`] with short critical
//! sections: reads never block each other for meaningful time, writers
//! never allocate on the hot path beyond the record clone. The workspace
//! otherwise prefers `parking_lot`; the registry is cold bookkeeping
//! (a handful of ops per second), and std keeps this crate's test suite
//! linkable in every environment the repo supports. Poison is recovered
//! (see `read_inner`/`write_inner`): a panicking writer cannot tear the
//! inner maps beyond what `snapshot` already tolerates.

use std::collections::HashMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::error::{RegistryError, TransitionError};
use crate::record::PluginRecord;
use crate::state::{PluginWireState, Transitionable};

/// Acquires a read guard, recovering from a poisoned lock.
///
/// Poison recovery means "the last writer panicked; the maps may hold a
/// half-written insert (see `insert`: order is pushed before the record
/// lands in the map)". `snapshot` iterates the order list and skips
/// missing ids, so a torn insert degrades to an invisible record, never
/// to garbage. Better than panicking every subsequent reader in a
/// long-lived proxy.
fn read_inner(inner: &RwLock<Inner>) -> RwLockReadGuard<'_, Inner> {
    inner
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Acquires a write guard, recovering from a poisoned lock. See
/// [`read_inner`] for the reasoning.
fn write_inner(inner: &RwLock<Inner>) -> RwLockWriteGuard<'_, Inner> {
    inner
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The allow-list gate every state move in this crate goes through.
///
/// Borrows the record, validates the `(from, to)` pair against
/// [`crate::ALLOWED_TRANSITIONS`] and applies the move. The record is
/// untouched when the table refuses.
fn checked_set_state(
    record: &mut PluginRecord,
    id: &str,
    target: PluginWireState,
) -> Result<PluginWireState, RegistryError> {
    let from = record.state();
    if !from.can_transition_to(target) {
        return Err(RegistryError::Transition {
            id: id.to_owned(),
            source: TransitionError::IllegalTransition { from, to: target },
        });
    }
    record.set_state(target);
    Ok(target)
}

#[derive(Default)]
struct Inner {
    records: HashMap<String, PluginRecord>,
    /// Insertion order, one entry per registered id; the list and snapshot
    /// iterate through this, not through the map.
    order: Vec<String>,
}

/// The in-memory registry of plugin records.
///
/// Cheap to construct, safe to share (`Send + Sync`), zero I/O. Records
/// are cloned in and out — the registry never hands out references that
/// would pin the lock.
#[derive(Default)]
pub struct PluginRegistry {
    inner: RwLock<Inner>,
}

impl PluginRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of registered records.
    pub fn len(&self) -> usize {
        read_inner(&self.inner).order.len()
    }

    /// Whether anything is registered at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Registers a freshly discovered record.
    ///
    /// The id already being registered is a hard error — silently
    /// shadowing a record would fork bookkeeping. Discovery-side
    /// duplicate detection happens earlier; this is the last gate.
    pub fn insert(&self, record: PluginRecord) -> Result<(), RegistryError> {
        let mut inner = write_inner(&self.inner);
        if inner.records.contains_key(&record.id) {
            return Err(RegistryError::DuplicatePlugin { id: record.id });
        }
        inner.order.push(record.id.clone());
        inner.records.insert(record.id.clone(), record);
        Ok(())
    }

    /// Finds a record by id, if registered.
    pub fn get(&self, id: &str) -> Option<PluginRecord> {
        read_inner(&self.inner).records.get(id).cloned()
    }

    /// Moves a record to `target`, checked against the allow-list.
    ///
    /// Returns the new state on success. Anything the table refuses is an
    /// error carrying the plugin id; the record is left untouched.
    pub fn set_state(
        &self,
        id: &str,
        target: PluginWireState,
    ) -> Result<PluginWireState, RegistryError> {
        let mut inner = write_inner(&self.inner);
        let record = inner
            .records
            .get_mut(id)
            .ok_or_else(|| RegistryError::UnknownPlugin { id: id.to_owned() })?;
        checked_set_state(record, id, target)
    }

    /// Sets (or replaces) the failure reason on a record, without moving
    /// its state.
    ///
    /// The failure path uses this when the failure is detected but the
    /// teardown has not finished yet; [`PluginRegistry::mark_failed`] then
    /// preserves the reason into terminal `UNLOADED`.
    pub fn set_reason(&self, id: &str, reason: impl Into<String>) -> Result<(), RegistryError> {
        let mut inner = write_inner(&self.inner);
        let record = inner
            .records
            .get_mut(id)
            .ok_or_else(|| RegistryError::UnknownPlugin { id: id.to_owned() })?;
        record.reason = Some(reason.into());
        Ok(())
    }

    /// Lands a record in terminal `UNLOADED` through the failure teardown.
    ///
    /// The record must be in `UNLOADING` (the single teardown funnel) —
    /// anything else is an [`RegistryError::UnexpectedState`] and the
    /// record is untouched. The failure reason recorded on the record is
    /// **preserved**, never cleared; if none was recorded, a generic
    /// placeholder is installed so terminal records are never silently
    /// blank in ps.
    pub fn mark_failed(&self, id: &str) -> Result<(), RegistryError> {
        let mut inner = write_inner(&self.inner);
        let record = inner
            .records
            .get_mut(id)
            .ok_or_else(|| RegistryError::UnknownPlugin { id: id.to_owned() })?;
        let found = record.state();
        if found != PluginWireState::Unloading {
            return Err(RegistryError::UnexpectedState {
                id: id.to_owned(),
                expected: PluginWireState::Unloading,
                found,
            });
        }
        // the allow-list gate is applied here too, not bypassed: if the
        // table ever loses this row, mark_failed fails loudly instead of
        // corrupting bookkeeping
        checked_set_state(record, id, PluginWireState::Unloaded)?;
        if record.reason.is_none() {
            record.reason = Some("marked failed without a reason".to_owned());
        }
        Ok(())
    }

    /// All registered records, in discovery (insertion) order.
    pub fn list(&self) -> Vec<PluginRecord> {
        self.snapshot().into_records()
    }

    /// A detached snapshot of the registry for ps/tree rendering.
    ///
    /// O(n) clone; the dump is meant to be handed to renderers and listed
    /// from without holding any lock. Use [`PluginRegistry::list`] when
    /// you just want the records.
    pub fn snapshot(&self) -> RegistrySnapshot {
        let inner = read_inner(&self.inner);
        let records = inner
            .order
            .iter()
            .filter_map(|id| inner.records.get(id).cloned())
            .collect();
        RegistrySnapshot { records }
    }
}

/// A detached, owned copy of the registry contents.
///
/// This is the shape ps/tree dumps consume: built on demand, sized by the
/// registered plugin count, free of locks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistrySnapshot {
    records: Vec<PluginRecord>,
}

impl RegistrySnapshot {
    /// The records, in discovery order.
    pub fn records(&self) -> &[PluginRecord] {
        &self.records
    }

    /// Number of records in the snapshot.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the snapshot is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Consumes the snapshot into the records vec.
    pub fn into_records(self) -> Vec<PluginRecord> {
        self.records
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::PluginManifest;

    fn demo_record(id: &str) -> PluginRecord {
        PluginRecord::from_manifest(
            &PluginManifest::new(id, "1.0.0", format!("{id} demo"), None),
            format!("plugins/{id}/{id}.dll"),
        )
    }

    #[test]
    fn insert_and_get_round_trip() {
        let registry = PluginRegistry::new();
        registry.insert(demo_record("greeting")).unwrap();
        let found = registry.get("greeting").expect("registered");
        assert_eq!(found.id, "greeting");
        assert_eq!(found.state(), PluginWireState::Discovered);
        assert_eq!(registry.get("nope"), None);
    }

    #[test]
    fn duplicate_insert_is_rejected() {
        let registry = PluginRegistry::new();
        registry.insert(demo_record("greeting")).unwrap();
        let err = registry.insert(demo_record("greeting")).unwrap_err();
        assert_eq!(
            err,
            RegistryError::DuplicatePlugin {
                id: "greeting".into()
            }
        );
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn legal_transition_moves_state() {
        let registry = PluginRegistry::new();
        registry.insert(demo_record("greeting")).unwrap();
        let next = registry
            .set_state("greeting", PluginWireState::Loading)
            .unwrap();
        assert_eq!(next, PluginWireState::Loading);
        assert_eq!(
            registry.get("greeting").unwrap().state(),
            PluginWireState::Loading
        );
    }

    #[test]
    fn illegal_transition_is_rejected_and_record_is_untouched() {
        let registry = PluginRegistry::new();
        registry.insert(demo_record("greeting")).unwrap();
        let err = registry
            .set_state("greeting", PluginWireState::Running)
            .unwrap_err();
        assert!(matches!(
            err,
            RegistryError::Transition {
                id,
                source: TransitionError::IllegalTransition {
                    from: PluginWireState::Discovered,
                    to: PluginWireState::Running,
                },
            } if id == "greeting"
        ));
        assert_eq!(
            registry.get("greeting").unwrap().state(),
            PluginWireState::Discovered
        );
    }

    #[test]
    fn unknown_id_errors_everywhere() {
        let registry = PluginRegistry::new();
        assert_eq!(
            registry.set_state("ghost", PluginWireState::Loading),
            Err(RegistryError::UnknownPlugin { id: "ghost".into() })
        );
        assert_eq!(
            registry.set_reason("ghost", "x"),
            Err(RegistryError::UnknownPlugin { id: "ghost".into() })
        );
        assert_eq!(
            registry.mark_failed("ghost"),
            Err(RegistryError::UnknownPlugin { id: "ghost".into() })
        );
    }

    #[test]
    fn mark_failed_lands_unloaded_and_preserves_reason() {
        let registry = PluginRegistry::new();
        registry.insert(demo_record("greeting")).unwrap();
        registry
            .set_state("greeting", PluginWireState::Unloading)
            .unwrap();
        registry
            .set_reason("greeting", "dlopen failed: missing symbol")
            .unwrap();
        registry.mark_failed("greeting").unwrap();
        let record = registry.get("greeting").unwrap();
        assert_eq!(record.state(), PluginWireState::Unloaded);
        assert_eq!(
            record.reason.as_deref(),
            Some("dlopen failed: missing symbol")
        );
    }

    #[test]
    fn mark_failed_installs_placeholder_when_no_reason_recorded() {
        let registry = PluginRegistry::new();
        registry.insert(demo_record("greeting")).unwrap();
        registry
            .set_state("greeting", PluginWireState::Unloading)
            .unwrap();
        registry.mark_failed("greeting").unwrap();
        let record = registry.get("greeting").unwrap();
        assert_eq!(record.state(), PluginWireState::Unloaded);
        assert_eq!(
            record.reason.as_deref(),
            Some("marked failed without a reason")
        );
    }

    #[test]
    fn mark_failed_requires_unloading() {
        let registry = PluginRegistry::new();
        registry.insert(demo_record("greeting")).unwrap();
        let err = registry.mark_failed("greeting").unwrap_err();
        assert_eq!(
            err,
            RegistryError::UnexpectedState {
                id: "greeting".into(),
                expected: PluginWireState::Unloading,
                found: PluginWireState::Discovered,
            }
        );
        assert_eq!(
            registry.get("greeting").unwrap().state(),
            PluginWireState::Discovered
        );
    }

    #[test]
    fn discovery_order_is_list_order() {
        let registry = PluginRegistry::new();
        for id in ["alpha", "beta", "gamma"] {
            registry.insert(demo_record(id)).unwrap();
        }
        let ids: Vec<String> = registry.list().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["alpha", "beta", "gamma"]);
    }

    #[test]
    fn snapshot_matches_list_and_is_detached() {
        let registry = PluginRegistry::new();
        registry.insert(demo_record("greeting")).unwrap();
        let snap = registry.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap.records()[0].id, "greeting");
        let owned = snap.into_records();
        assert_eq!(owned.len(), 1);
        assert_eq!(registry.list(), owned);
    }

    #[test]
    fn empty_registry_is_honest() {
        let registry = PluginRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.list().is_empty());
        assert!(registry.snapshot().is_empty());
    }
}
