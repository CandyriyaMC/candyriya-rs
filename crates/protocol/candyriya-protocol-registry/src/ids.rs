//! Parsing and validation of the packet-id tables.
//!
//! `protocol_ids.json` (schema 1) holds, per protocol id, the packet ids
//! used in the status and login states. Slot keys are the [`PacketSlot`]
//! `as_str` values; all slots must be present, either as a number or as an
//! explicit `null` covered by a `known_gaps` entry.

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;

use crate::state::PacketSlot;

const SLOT_COUNT: usize = PacketSlot::ALL.len();

/// The parsed, validated packet-id tables, sorted by protocol id.
#[derive(Debug)]
pub(crate) struct ProtocolIds {
    entries: Box<[IdsEntry]>,
}

#[derive(Debug)]
struct IdsEntry {
    protocol: i32,
    /// Release names this entry is known to apply to. Informational; used to
    /// cross-check the versions table and never for classification.
    names: Box<[String]>,
    slots: [Option<i32>; SLOT_COUNT],
    #[allow(dead_code)]
    source_note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IdsFileJson {
    schema: u32,
    #[serde(default)]
    known_gaps: Vec<KnownGapJson>,
    protocols: Vec<ProtocolIdsJson>,
}

#[derive(Debug, Deserialize)]
struct KnownGapJson {
    protocols: Vec<i32>,
    slot: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct ProtocolIdsJson {
    protocol: i32,
    names: Vec<String>,
    ids: BTreeMap<String, Option<i64>>,
    #[serde(default)]
    source_note: Option<String>,
}

impl ProtocolIds {
    /// The packet id for a slot in a protocol, `None` when the slot is a
    /// known gap on that protocol or the protocol has no table at all.
    pub(crate) fn get(&self, protocol: i32, slot: PacketSlot) -> Option<i32> {
        let index = self
            .entries
            .binary_search_by(|e| e.protocol.cmp(&protocol))
            .ok()?;
        self.entries[index].slots[slot.index()]
    }

    /// Whether any table exists for this protocol.
    pub(crate) fn covers(&self, protocol: i32) -> bool {
        self.entries
            .binary_search_by(|e| e.protocol.cmp(&protocol))
            .is_ok()
    }

    /// Every release name mentioned across all entries; used by the pair
    /// integrity check to spot names the active versions table does not know.
    pub(crate) fn extra_names(&self, is_known: impl Fn(&str) -> bool) -> Vec<String> {
        let mut out = Vec::new();
        for entry in self.entries.iter() {
            for name in entry.names.iter() {
                if !is_known(name) {
                    out.push(name.clone());
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Parse and validate a `protocol_ids.json` document.
pub(crate) fn validate(bytes: &[u8]) -> Result<ProtocolIds, String> {
    let raw: IdsFileJson =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid JSON or schema: {e}"))?;
    if raw.schema != 1 {
        return Err(format!("unsupported schema {}: expected 1", raw.schema));
    }
    if raw.protocols.is_empty() {
        return Err("packet-id table is empty".to_string());
    }

    // known_gaps first, so per-entry checks can consult them
    let mut gaps: HashSet<(i32, PacketSlot)> = HashSet::new();
    for (index, gap) in raw.known_gaps.iter().enumerate() {
        if gap.reason.trim().is_empty() {
            return Err(format!("known_gaps[{index}]: empty reason"));
        }
        let slot = PacketSlot::from_key(&gap.slot)
            .ok_or_else(|| format!("known_gaps[{index}]: unknown slot {:?}", gap.slot))?;
        if !PacketSlot::NULLABLE.contains(&slot) {
            return Err(format!(
                "known_gaps[{index}]: slot {slot} is not allowed to have a gap"
            ));
        }
        if gap.protocols.is_empty() {
            return Err(format!("known_gaps[{index}]: empty protocols list"));
        }
        let mut dedup = HashSet::with_capacity(gap.protocols.len());
        for &protocol in &gap.protocols {
            if protocol < 0 {
                return Err(format!(
                    "known_gaps[{index}]: protocol {protocol} is negative"
                ));
            }
            if !dedup.insert(protocol) {
                return Err(format!(
                    "known_gaps[{index}]: protocol {protocol} listed twice"
                ));
            }
            if !gaps.insert((protocol, slot)) {
                return Err(format!(
                    "known_gaps[{index}]: protocol {protocol} / {slot} listed twice"
                ));
            }
        }
    }

    let mut protocols = HashSet::with_capacity(raw.protocols.len());
    for (index, entry) in raw.protocols.iter().enumerate() {
        if entry.protocol < 0 {
            return Err(format!(
                "protocols[{index}]: protocol {} is negative",
                entry.protocol
            ));
        }
        if !protocols.insert(entry.protocol) {
            return Err(format!(
                "protocols[{index}]: duplicate protocol {}",
                entry.protocol
            ));
        }
        if entry.names.is_empty() {
            return Err(format!("protocols[{index}]: names must not be empty"));
        }
        let mut dedup = HashSet::with_capacity(entry.names.len());
        for name in &entry.names {
            if !is_release_name(name) {
                return Err(format!(
                    "protocols[{index}]: name {name:?} is not a stable release name"
                ));
            }
            if !dedup.insert(name) {
                return Err(format!("protocols[{index}]: duplicate name {name:?}"));
            }
        }
    }

    // every known_gaps protocol must have a corresponding entry; a gap that
    // points nowhere would otherwise pass silently
    for (index, gap) in raw.known_gaps.iter().enumerate() {
        for &protocol in &gap.protocols {
            if !protocols.contains(&protocol) {
                return Err(format!(
                    "known_gaps[{index}]: protocol {protocol} has no entry in protocols"
                ));
            }
        }
    }

    let mut entries = Vec::with_capacity(raw.protocols.len());
    for (index, entry) in raw.protocols.into_iter().enumerate() {
        let protocol = entry.protocol;
        // exactly the twelve known slots, nothing else
        let mut slots: [Option<i32>; SLOT_COUNT] = [None; SLOT_COUNT];
        let mut seen = HashSet::with_capacity(SLOT_COUNT);
        for (key, value) in &entry.ids {
            let slot = PacketSlot::from_key(key).ok_or_else(|| {
                format!("protocols[{index}] ({protocol}): unknown slot key {key:?}")
            })?;
            seen.insert(slot);
            if let Some(v) = *value {
                if !(0..=i64::from(i32::MAX)).contains(&v) {
                    return Err(format!(
                        "protocols[{index}] ({protocol}): slot {slot} id {v} is outside 0..=2147483647"
                    ));
                }
                slots[slot.index()] = Some(v as i32);
            } else {
                if !PacketSlot::NULLABLE.contains(&slot) {
                    return Err(format!(
                        "protocols[{index}] ({protocol}): slot {slot} may not be null"
                    ));
                }
                slots[slot.index()] = None;
            }
        }
        for slot in PacketSlot::ALL {
            if !seen.contains(slot) {
                return Err(format!(
                    "protocols[{index}] ({protocol}): missing slot {slot}"
                ));
            }
        }
        // nulls must be exactly the known gaps for this protocol
        for slot in PacketSlot::NULLABLE {
            let is_null = slots[slot.index()].is_none();
            let is_gap = gaps.contains(&(protocol, *slot));
            if is_null != is_gap {
                return Err(format!(
                    "protocols[{index}] ({protocol}): slot {slot} is {} but known_gaps says {}",
                    if is_null { "null" } else { "numeric" },
                    if is_gap { "gap" } else { "no gap" }
                ));
            }
        }
        // ids inside one direction+state group must be pairwise distinct
        for group in [
            &[
                PacketSlot::StatusServerboundRequest,
                PacketSlot::StatusServerboundPing,
            ][..],
            &[
                PacketSlot::StatusClientboundResponse,
                PacketSlot::StatusClientboundPong,
            ][..],
            &[
                PacketSlot::LoginServerboundStart,
                PacketSlot::LoginServerboundEncryptionResponse,
                PacketSlot::LoginServerboundPluginResponse,
            ][..],
            &[
                PacketSlot::LoginClientboundDisconnect,
                PacketSlot::LoginClientboundEncryptionRequest,
                PacketSlot::LoginClientboundLoginSuccess,
                PacketSlot::LoginClientboundSetCompression,
                PacketSlot::LoginClientboundPluginRequest,
            ][..],
        ] {
            let mut group_ids = Vec::new();
            for slot in group {
                if let Some(id) = slots[slot.index()] {
                    group_ids.push(id);
                }
            }
            group_ids.sort_unstable();
            let unique = group_ids.iter().collect::<HashSet<_>>().len();
            if unique != group_ids.len() {
                return Err(format!(
                    "protocols[{index}] ({protocol}): duplicate packet id inside a direction group"
                ));
            }
        }
        entries.push(IdsEntry {
            protocol,
            names: entry.names.into_boxed_slice(),
            slots,
            source_note: entry.source_note,
        });
    }
    entries.sort_by_key(|e| e.protocol);

    Ok(ProtocolIds {
        entries: entries.into_boxed_slice(),
    })
}

/// A stable release name: `M.m[.p]`, all numeric components.
fn is_release_name(name: &str) -> bool {
    let mut parts = name.split('.');
    let component_ok = |p: Option<&str>| -> bool {
        p.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
    };
    let major = parts.next();
    let minor = parts.next();
    let patch = parts.next();
    parts.next().is_none()
        && component_ok(major)
        && component_ok(minor)
        && match patch {
            Some(p) => component_ok(Some(p)),
            None => true,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMBEDDED: &[u8] = include_bytes!("../protocol_ids.json");

    #[test]
    fn embedded_snapshot_validates() {
        let ids = validate(EMBEDDED).expect("embedded packet ids must validate");
        assert_eq!(ids.entries.len(), 52);
        assert!(
            ids.entries
                .windows(2)
                .all(|w| w[0].protocol < w[1].protocol)
        );
        // every supported release protocol is covered
        for protocol in [
            4, 5, 47, 107, 108, 109, 110, 210, 315, 316, 335, 338, 340, 393, 401, 404, 477, 480,
            485, 490, 498, 573, 575, 578, 735, 736, 751, 753, 754, 755, 756, 757, 758, 759, 760,
            761, 762, 763, 764, 765, 766, 767, 768, 769, 770, 771, 772, 773, 774, 775, 776, 777,
        ] {
            assert!(ids.covers(protocol), "missing protocol {protocol}");
        }
    }

    #[test]
    fn rejects_unknown_slot_key() {
        let raw = br#"{
            "schema": 1,
            "protocols": [{
                "protocol": 4,
                "names": ["1.7.2"],
                "ids": {"status.serverbound.request": 0, "bogus.slot": 1}
            }]
        }"#;
        assert!(validate(raw).is_err());
    }

    #[test]
    fn rejects_gap_pointing_at_missing_protocol() {
        // gaps for 47 are consistent (the nulls exist), but the extra gap
        // for 48 points at a protocol with no entry
        let raw = br#"{
            "schema": 1,
            "known_gaps": [
                {
                    "protocols": [47],
                    "slot": "login.serverbound.plugin_response",
                    "reason": "login plugin messages introduced in 1.13"
                },
                {
                    "protocols": [47],
                    "slot": "login.clientbound.plugin_request",
                    "reason": "login plugin messages introduced in 1.13"
                },
                {
                    "protocols": [48],
                    "slot": "login.clientbound.set_compression",
                    "reason": "compression introduced in 1.8"
                }
            ],
            "protocols": [{
                "protocol": 47,
                "names": ["1.8"],
                "ids": {
                    "status.serverbound.request": 0,
                    "status.serverbound.ping": 1,
                    "status.clientbound.response": 0,
                    "status.clientbound.pong": 1,
                    "login.serverbound.start": 0,
                    "login.serverbound.encryption_response": 1,
                    "login.serverbound.plugin_response": null,
                    "login.clientbound.disconnect": 0,
                    "login.clientbound.encryption_request": 1,
                    "login.clientbound.login_success": 2,
                    "login.clientbound.set_compression": 3,
                    "login.clientbound.plugin_request": null
                }
            }]
        }"#;
        assert!(validate(raw).is_err());
    }

    #[test]
    fn rejects_null_without_gap() {
        let raw = br#"{
            "schema": 1,
            "protocols": [{
                "protocol": 47,
                "names": ["1.8"],
                "ids": {
                    "status.serverbound.request": 0,
                    "status.serverbound.ping": 1,
                    "status.clientbound.response": 0,
                    "status.clientbound.pong": 1,
                    "login.serverbound.start": 0,
                    "login.serverbound.encryption_response": 1,
                    "login.serverbound.plugin_response": null,
                    "login.clientbound.disconnect": 0,
                    "login.clientbound.encryption_request": 1,
                    "login.clientbound.login_success": 2,
                    "login.clientbound.set_compression": 3,
                    "login.clientbound.plugin_request": null
                }
            }]
        }"#;
        assert!(validate(raw).is_err());
    }
}
