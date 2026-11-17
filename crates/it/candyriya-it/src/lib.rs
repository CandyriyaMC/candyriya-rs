//! # candyriya-it
//!
//! Integration scaffolding for the candyriya crates: a **real**, loopback,
//! server-side TCP backend and a real client speaking the Minecraft Java
//! Edition protocol over it. No mocks, no stubs, no fake kernels — the
//! bytes below the socket are the actual protocol, encoded and decoded with
//! `candyriya-protocol`, and the packet ids come from
//! `candyriya-protocol-registry`.
//!
//! ## What lives here
//!
//! * [`FakeBackendConfig`] / [`BackendFixture`] — an independent
//!   server-side implementation that accepts clients, completes the
//!   handshake, serves status (MOTD / max players / favicon / protocol
//!   echo) and login (offline flow), and holds the connection open until
//!   the client drops. See [`BackendFixture`] for the exact wire behavior,
//!   including what happens with unknown protocol numbers, unknown intents
//!   and unknown packet ids.
//! * [`TestClient`] / [`LoginSuccess`] — a client performing handshake,
//!   status ping, login start and chat send, usable to drive the backend
//!   (and, later, the proxy) in integration tests.
//! * [`BackendStatsSnapshot`] — counters from the backend, so tests can
//!   assert on how many connections were served and how many frames were
//!   discarded.
//! * [`format_uuid`] / [`parse_uuid`] / [`uuid_from_name`] — UUID text
//!   helpers shared by the harness and the tests.
//!
//! ## Fixture lifecycle
//!
//! [`BackendFixture::start`] binds `127.0.0.1:0` and reads back the
//! assigned ephemeral port; tests never touch any port by hand. Every
//! fixture owns its accept loop and connection tasks; [`BackendFixture::close`]
//! joins them deterministically, and the [`Drop`](BackendFixture) impl aborts
//! them as a safety net. No orphaned loops or held sockets survive either
//! path.
//!
//! ## Network scope
//!
//! Loopback only. The backend binds `127.0.0.1`, the client connects to
//! wherever the fixture tells it to, and nothing else on the network is
//! reachable from here. There is no TLS; framing compression is negotiated
//! by the client (Set Compression flips the link), and encryption is never
//! negotiated.
#![forbid(unsafe_code)]

mod backend;
mod client;
mod error;
mod json;
mod uuid;
mod wire;

pub use crate::backend::{
    BackendFixture, BackendForwarding, BackendStatsSnapshot, FakeBackendConfig,
};
pub use crate::client::{LoginSuccess, TestClient};
pub use crate::error::HarnessError;
pub use crate::uuid::{UuidParseError, format_uuid, parse_uuid, uuid_from_name};

pub use candyriya_protocol::{Intent, Packet};

use candyriya_protocol_registry::{PacketSlot, Registry, VersionInfo};

/// Resolve the table entry used to classify packets for `protocol`.
///
/// The protocol number carried by a client handshake is never validated. If
/// the registry has no table for it (very new, very old or garbage numbers),
/// the newest known release is used — the harness is a stand-in for a server
/// and does not reject clients for version reasons.
pub(crate) fn resolve_version(registry: &Registry, protocol: i32) -> &'static VersionInfo {
    registry
        .from_protocol(protocol)
        .unwrap_or_else(|| registry.latest())
}

/// Resolve the packet id for `slot` at `protocol`.
///
/// Falls back to the newest known release when the protocol is absent from
/// the registry (see [`resolve_version`]); returns an error only if the
/// registry cannot cover even that, which would be a build bug.
pub(crate) fn protocol_id(
    registry: &Registry,
    protocol: i32,
    slot: PacketSlot,
) -> Result<i32, HarnessError> {
    registry
        .packet_id(protocol, slot)
        .or_else(|| registry.packet_id(registry.latest().protocol, slot))
        .ok_or_else(|| {
            HarnessError::UnexpectedData(format!(
                "registry has no packet id for slot {slot} (neither protocol {protocol} \
                 nor the latest known release {})",
                registry.latest().protocol
            ))
        })
}
