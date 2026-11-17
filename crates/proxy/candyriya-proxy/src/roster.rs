//! The session roster: who is online, on which backend, right now.
//!
//! The proxy terminates the login exchange and becomes a byte pump in the
//! play state, so "who is online" cannot be answered by the backend — the
//! only place that knows is this proxy. The roster is populated the moment
//! a session transitions to play and drained when the byte pump ends (or
//! the connection task is dropped), so it is authoritative for the live
//! population.
//!
//! The data is informational: `/glist`, the status snapshot and the
//! metrics all read it. Nothing in the hot path (the byte pump) holds the
//! lock — insertion and removal happen once per session, both outside the
//! copy loop.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use uuid::Uuid;

/// One live play-state session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterEntry {
    /// Connection id of the session (`conn_id` in the logs); the roster key.
    pub conn_id: u64,
    /// Authenticated profile id as confirmed by the auth layer.
    pub uuid: Uuid,
    /// Player name, taken from the profile the auth layer resolved (the
    /// session server can hand back a different casing than the client
    /// sent).
    pub name: String,
    /// Backend the player landed on. `None` only if a future code path
    /// inserts a session before routing decided — treated as "between
    /// servers" by consumers.
    pub server: Option<String>,
    /// When the transition to play happened. Used to break snapshot ties
    /// deterministically; rendering sorts by `(name, conn_id)` instead.
    pub joined: Instant,
}

/// The live roster. Cheap to clone nothing — always used behind `Arc`.
#[derive(Debug, Default)]
pub struct PlayerRoster {
    entries: Mutex<HashMap<u64, RosterEntry>>,
}

impl PlayerRoster {
    /// An empty roster.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a session that transitioned to play.
    ///
    /// Inserting twice under the same `conn_id` replaces the previous
    /// entry and returns it; the id was already recycled by the counter,
    /// so the old row is stale either way.
    pub fn insert(
        &self,
        conn_id: u64,
        uuid: Uuid,
        name: impl Into<String>,
        server: Option<String>,
    ) -> Option<RosterEntry> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                conn_id,
                RosterEntry {
                    conn_id,
                    uuid,
                    name: name.into(),
                    server,
                    joined: Instant::now(),
                },
            )
    }

    /// Drop a session (play state ended or the task went away).
    ///
    /// Returns the entry that was removed, if any.
    pub fn remove(&self, conn_id: u64) -> Option<RosterEntry> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&conn_id)
    }

    /// A point-in-time view of every live session, sorted by `(name,
    /// conn_id)` so command output is stable between invocations.
    #[must_use]
    pub fn snapshot(&self) -> Vec<RosterEntry> {
        let mut entries: Vec<RosterEntry> = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name).then(a.conn_id.cmp(&b.conn_id)));
        entries
    }

    /// Number of live sessions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether no session is live right now.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// RAII guard removing the session from the roster when the connection
/// task ends, however it ends (clean pump exit, error, or task drop).
///
/// The login flow installs one right after the transition to play; while
/// it is held, the player counts as online everywhere.
pub(crate) struct RosterGuard<'a> {
    roster: &'a PlayerRoster,
    conn_id: u64,
}

impl<'a> RosterGuard<'a> {
    pub(crate) fn new(roster: &'a PlayerRoster, conn_id: u64) -> Self {
        Self { roster, conn_id }
    }
}

impl Drop for RosterGuard<'_> {
    fn drop(&mut self) {
        self.roster.remove(self.conn_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn insert_and_snapshot_roundtrip() {
        let roster = PlayerRoster::new();
        assert!(roster.is_empty());

        roster.insert(1, uuid(0xAAAA), "Steve", Some("local".to_owned()));
        roster.insert(2, uuid(0xBBBB), "Alex", Some("nether".to_owned()));
        roster.insert(3, uuid(0xCCCC), "Alex", None);

        let snapshot = roster.snapshot();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(roster.len(), 3);

        // sorted by (name, conn_id): Alex@2, Alex@3, Steve@1
        let names: Vec<(u64, &str)> = snapshot
            .iter()
            .map(|e| (e.conn_id, e.name.as_str()))
            .collect();
        assert_eq!(names, [(2, "Alex"), (3, "Alex"), (1, "Steve")]);
        assert_eq!(snapshot[0].server.as_deref(), Some("nether"));
        assert_eq!(snapshot[1].server, None);
        assert_eq!(snapshot[2].uuid, uuid(0xAAAA));
    }

    #[test]
    fn remove_drops_the_session_and_reports_it() {
        let roster = PlayerRoster::new();
        roster.insert(7, uuid(0xD00D), "Steve", Some("local".to_owned()));
        let removed = roster.remove(7).expect("entry was present");
        assert_eq!(removed.name, "Steve");
        assert!(roster.remove(7).is_none());
        assert!(roster.is_empty());
    }

    #[test]
    fn reinsert_under_the_same_conn_id_replaces_the_entry() {
        let roster = PlayerRoster::new();
        roster.insert(9, uuid(0x1111), "Steve", None);
        let previous = roster.insert(9, uuid(0x2222), "Alex", Some("local".to_owned()));
        assert_eq!(previous.map(|entry| entry.uuid), Some(uuid(0x1111)));
        let snapshot = roster.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].uuid, uuid(0x2222));
    }

    #[test]
    fn guard_removes_the_session_on_drop() {
        let roster = PlayerRoster::new();
        roster.insert(4, uuid(0xF00D), "Steve", Some("local".to_owned()));
        {
            let _guard = RosterGuard::new(&roster, 4);
            assert_eq!(roster.len(), 1);
        }
        assert_eq!(roster.len(), 0);
    }
}
