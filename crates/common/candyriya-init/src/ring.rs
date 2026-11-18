//! Bounded history of unit exits.
//!
//! The registry answers "what is alive right now"; the ring answers "what
//! recently died". It keeps at most `capacity` [`ExitRecord`]s in FIFO
//! order and counts how many it had to discard along the way, so operators
//! can tell a full ring from a quiet system.

use std::collections::VecDeque;

use crate::exit::ExitRecord;

/// A bounded FIFO of recent unit exits.
///
/// Pushing into a full ring drops the oldest record. `capacity` is clamped
/// to at least 1 so a misconfigured tree never has a disabled ring.
#[derive(Debug)]
pub struct RecentExits {
    records: VecDeque<ExitRecord>,
    capacity: usize,
    total_recorded: u64,
}

impl RecentExits {
    /// Create a ring holding up to `capacity` records (at least 1).
    pub(crate) fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            records: VecDeque::with_capacity(capacity.min(1024)),
            capacity,
            total_recorded: 0,
        }
    }

    /// Record an exit, evicting the oldest record if the ring is full.
    pub(crate) fn push(&mut self, record: ExitRecord) {
        if self.records.len() == self.capacity {
            self.records.pop_front();
        }
        self.records.push_back(record);
        self.total_recorded = self.total_recorded.saturating_add(1);
    }

    /// Number of records currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// The configured capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Every exit ever recorded, including those evicted from the ring.
    /// `total_recorded - len()` is the number of evicted records.
    #[must_use]
    pub fn total_recorded(&self) -> u64 {
        self.total_recorded
    }

    /// Iterate over the held records, oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &ExitRecord> {
        self.records.iter()
    }

    /// Snapshot the `limit` most recent records, oldest first.
    ///
    /// `limit == 0` yields an empty vec.
    #[must_use]
    pub fn recent(&self, limit: usize) -> Vec<ExitRecord> {
        let start = self.len().saturating_sub(limit);
        self.iter().skip(start).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::ExitStatus;
    use crate::node::UnitKind;
    use crate::pid::Pid;
    use std::time::Instant;

    fn record(pid: u64) -> ExitRecord {
        ExitRecord {
            pid: Pid::new(pid),
            ppid: Pid::INIT,
            name: format!("unit-{pid}"),
            kind: UnitKind::Oneshot,
            status: ExitStatus::Success,
            restart_count: 0,
            started_at: Instant::now(),
            exited_at: Instant::now(),
        }
    }

    #[test]
    fn ring_evicts_oldest_when_full() {
        let mut ring = RecentExits::new(3);
        for pid in 1..=5 {
            ring.push(record(pid));
        }
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.total_recorded(), 5);
        let pids: Vec<u64> = ring.iter().map(|r| r.pid.as_u64()).collect();
        assert_eq!(pids, [3, 4, 5]);
    }

    #[test]
    fn recent_returns_newest_in_chronological_order() {
        let mut ring = RecentExits::new(8);
        for pid in 1..=6 {
            ring.push(record(pid));
        }
        let pids: Vec<u64> = ring.recent(3).iter().map(|r| r.pid.as_u64()).collect();
        assert_eq!(pids, [4, 5, 6]);
        assert_eq!(ring.recent(0).len(), 0);
        assert_eq!(ring.recent(100).len(), 6);
    }

    #[test]
    fn zero_capacity_is_clamped() {
        let mut ring = RecentExits::new(0);
        ring.push(record(1));
        assert_eq!(ring.capacity(), 1);
        assert_eq!(ring.len(), 1);
    }
}
