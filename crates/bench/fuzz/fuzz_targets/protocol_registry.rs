//! Fuzz the protocol registry's lookup and classification paths.
//!
//! The registry maps (state, direction, packet id) to a packet kind using
//! compile-time-embedded tables. Lookups must not panic and must stay
//! consistent with what the embedded tables say, otherwise a weird-enough
//! client frame could wedge the classifier.
//!
//! Layout of the input bytes:
//!
//! ```text
//! byte  0      connection state selector (mod 5)
//! byte  1      bit 0: direction (0 = serverbound, 1 = clientbound)
//! bytes 2..6   protocol version, i32 LE
//! bytes 6..10  packet id, i32 LE
//! bytes 10..   release name probe (arbitrary bytes, lossy-decoded)
//! ```

#![no_main]

use std::sync::OnceLock;

use candyriya_protocol_registry::{ConnectionState, Direction, PacketKind, PacketSlot, Registry};
use libfuzzer_sys::fuzz_target;

fn registry() -> Option<&'static Registry> {
    static REGISTRY: OnceLock<Option<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Registry::embedded().ok()).as_ref()
}

fn state_from_byte(byte: u8) -> ConnectionState {
    match byte % 5 {
        0 => ConnectionState::Handshaking,
        1 => ConnectionState::Status,
        2 => ConnectionState::Login,
        3 => ConnectionState::Configuration,
        _ => ConnectionState::Play,
    }
}

fuzz_target!(|data: &[u8]| {
    let Some(registry) = registry() else {
        // the embedded snapshot failing to load is a build bug, not a
        // reason to crash the fuzzer; there is nothing to classify
        return;
    };

    if data.len() < 10 {
        return;
    }
    let state = state_from_byte(data[0]);
    let direction = if data[1] & 1 == 0 {
        Direction::Serverbound
    } else {
        Direction::Clientbound
    };
    let protocol = i32::from_le_bytes([data[2], data[3], data[4], data[5]]);
    let packet_id = i32::from_le_bytes([data[6], data[7], data[8], data[9]]);

    // unknown protocol ids fall back to the latest tracked release; the
    // classifier must cope with a protocol it has no table for
    let version = registry
        .from_protocol(protocol)
        .unwrap_or_else(|| registry.latest());
    let kind = registry.classify(state, direction, version, packet_id);

    // the classifier documents exactly which states it tracks; the rest
    // always come back Unknown, carrying the raw id verbatim
    if let PacketKind::Unknown(id) = kind {
        assert_eq!(id, packet_id);
    }
    match (state, direction) {
        (ConnectionState::Handshaking, Direction::Serverbound) => {
            if packet_id == 0 {
                assert!(matches!(kind, PacketKind::Handshake));
            } else {
                assert!(matches!(kind, PacketKind::Unknown(_)));
            }
        }
        (ConnectionState::Handshaking, Direction::Clientbound)
        | (ConnectionState::Configuration, _)
        | (ConnectionState::Play, _) => {
            assert!(matches!(kind, PacketKind::Unknown(_)));
        }
        _ => {}
    }

    // every slot lookup must be panic-free and self-describing
    for slot in PacketSlot::ALL {
        let _ = registry.packet_id(protocol, *slot);
        let _ = registry.packet_id(version.protocol, *slot);
    }

    // arbitrary bytes as a release-name probe; a hit must roundtrip through
    // the name -> protocol lookup consistently
    let probe = String::from_utf8_lossy(&data[10..]);
    if let Some(found) = registry.from_name(&probe) {
        assert_eq!(registry.protocol_id_of(found.name), Some(found.protocol));
    }

    // arbitrary bytes as a slot key; a hit must be the canonical spelling
    if let Some(slot) = PacketSlot::from_key(&probe) {
        assert_eq!(slot.as_str(), probe);
    }

    // from_protocol must agree with from_name for anything it finds
    if let Some(found) = registry.from_protocol(protocol) {
        assert_eq!(found.protocol, protocol);
    }
});
