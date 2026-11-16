//! The packet classifier.
//!
//! Kept separate from [`crate::loader`] so the mapping from slots to kinds
//! stays in one place.

use crate::ids::ProtocolIds;
use crate::kind::PacketKind;
use crate::state::{ConnectionState, Direction, PacketSlot};

/// Slot-to-kind resolution for one protocol; `None` slot ids (a known gap)
/// never match any wire id.
fn slot_kind(
    ids: &ProtocolIds,
    protocol: i32,
    packet_id: i32,
    slot: PacketSlot,
    kind: PacketKind,
) -> Option<PacketKind> {
    (ids.get(protocol, slot) == Some(packet_id)).then_some(kind)
}

pub(crate) fn classify(
    ids: &ProtocolIds,
    state: ConnectionState,
    direction: Direction,
    protocol: i32,
    packet_id: i32,
) -> PacketKind {
    match (state, direction) {
        (ConnectionState::Handshaking, Direction::Serverbound) => {
            // not part of the packet-id tables; constant across every release
            if packet_id == 0 {
                PacketKind::Handshake
            } else {
                PacketKind::Unknown(packet_id)
            }
        }
        (ConnectionState::Status, Direction::Serverbound) => slot_kind(
            ids,
            protocol,
            packet_id,
            PacketSlot::StatusServerboundRequest,
            PacketKind::StatusRequest,
        )
        .or_else(|| {
            slot_kind(
                ids,
                protocol,
                packet_id,
                PacketSlot::StatusServerboundPing,
                PacketKind::StatusPing,
            )
        })
        .unwrap_or(PacketKind::Unknown(packet_id)),
        (ConnectionState::Status, Direction::Clientbound) => slot_kind(
            ids,
            protocol,
            packet_id,
            PacketSlot::StatusClientboundResponse,
            PacketKind::StatusResponse,
        )
        .or_else(|| {
            slot_kind(
                ids,
                protocol,
                packet_id,
                PacketSlot::StatusClientboundPong,
                PacketKind::StatusPong,
            )
        })
        .unwrap_or(PacketKind::Unknown(packet_id)),
        (ConnectionState::Login, Direction::Serverbound) => slot_kind(
            ids,
            protocol,
            packet_id,
            PacketSlot::LoginServerboundStart,
            PacketKind::LoginStart,
        )
        .or_else(|| {
            slot_kind(
                ids,
                protocol,
                packet_id,
                PacketSlot::LoginServerboundEncryptionResponse,
                PacketKind::EncryptionResponse,
            )
        })
        .or_else(|| {
            slot_kind(
                ids,
                protocol,
                packet_id,
                PacketSlot::LoginServerboundPluginResponse,
                PacketKind::LoginPluginResponse,
            )
        })
        .unwrap_or(PacketKind::Unknown(packet_id)),
        (ConnectionState::Login, Direction::Clientbound) => slot_kind(
            ids,
            protocol,
            packet_id,
            PacketSlot::LoginClientboundDisconnect,
            PacketKind::LoginDisconnect,
        )
        .or_else(|| {
            slot_kind(
                ids,
                protocol,
                packet_id,
                PacketSlot::LoginClientboundEncryptionRequest,
                PacketKind::EncryptionRequest,
            )
        })
        .or_else(|| {
            slot_kind(
                ids,
                protocol,
                packet_id,
                PacketSlot::LoginClientboundLoginSuccess,
                PacketKind::LoginSuccess,
            )
        })
        .or_else(|| {
            slot_kind(
                ids,
                protocol,
                packet_id,
                PacketSlot::LoginClientboundSetCompression,
                PacketKind::SetCompression,
            )
        })
        .or_else(|| {
            slot_kind(
                ids,
                protocol,
                packet_id,
                PacketSlot::LoginClientboundPluginRequest,
                PacketKind::LoginPluginRequest,
            )
        })
        .unwrap_or(PacketKind::Unknown(packet_id)),
        // handshake-to-client, configuration, play: not tracked
        _ => PacketKind::Unknown(packet_id),
    }
}
