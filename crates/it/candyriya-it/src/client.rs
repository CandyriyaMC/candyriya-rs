//! A real Minecraft client over a raw TCP stream.
//!
//! Every method speaks the actual wire protocol through `candyriya-protocol`
//! (or `candyriya-protocol-registry` for per-version packet ids). Nothing is
//! stubbed: the bytes go onto the socket verbatim, and the client fails on
//! unexpected ids instead of silently ignoring them. The client owns one
//! reusable read buffer, never blocks longer than the socket itself would,
//! and flips to zlib-framed packets the moment a Set Compression packet is
//! observed — the way a real client does.
//!
//! Packet ids are resolved from the registry for the given protocol version;
//! for protocol numbers the registry does not know, the ids of the newest
//! known release are used (see [`protocol_id`]). The client never validates
//! the protocol number itself.

use bytes::{Bytes, BytesMut};
use candyriya_compression::Compression;
use candyriya_protocol::{
    Handshake, Intent, Packet, decode_varint, read_string, read_uuid, write_string, write_uuid,
};
use candyriya_protocol_registry::{PacketSlot, Registry};
use tokio::net::TcpStream;

use crate::uuid::{format_uuid, parse_uuid, uuid_from_name};
use crate::{HarnessError, protocol_id, wire};

/// A decoded login success as seen by a client.
///
/// The raw wire body is `UUID | String(16) username | ...`; the trailing
/// bytes (properties and, since 26.2, the session id) are preserved verbatim
/// in [`raw_extra`](LoginSuccess::raw_extra) so tests can assert on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSuccess {
    uuid: (i64, i64),
    name: String,
    extra: Bytes,
}

impl LoginSuccess {
    /// The UUID as raw big-endian halves, `(msb, lsb)`.
    pub fn uuid_parts(&self) -> (i64, i64) {
        self.uuid
    }

    /// The UUID in canonical hyphenated lowercase text form.
    pub fn uuid_string(&self) -> String {
        format_uuid(self.uuid)
    }

    /// Whether the UUID is anything other than all zeros.
    pub fn is_uuid_non_zero(&self) -> bool {
        self.uuid != (0, 0)
    }

    /// Whether [`uuid_string`](LoginSuccess::uuid_string) round-trips back
    /// through the canonical parser to the same halves.
    pub fn is_uuid_parseable(&self) -> bool {
        parse_uuid(&self.uuid_string()) == Ok(self.uuid)
    }

    /// The username exactly as it was sent on the wire.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Any bytes after the username (properties, session id), verbatim.
    pub fn raw_extra(&self) -> &Bytes {
        &self.extra
    }
}

/// A client speaking the Minecraft wire protocol over a raw TCP stream.
///
/// Construct with [`TestClient::connect`]. Each flow method handles one step;
/// they compose into the full status / login flows used by the integration
/// tests. The client never times anything out on its own — wrap calls in
/// `tokio::time::timeout` when a negative wait is part of the assertion.
///
/// The client tracks framing compression the way a real one does: once a
/// Set Compression packet is observed, every following frame in both
/// directions is zlib-framed. The plain layout is used before that point.
/// [`compression_threshold`](TestClient::compression_threshold) reports the
/// negotiated threshold, or `None` while compression is off.
pub struct TestClient {
    stream: TcpStream,
    buf: BytesMut,
    registry: Registry,
    compression: Option<Compression>,
}

impl std::fmt::Debug for TestClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestClient")
            .field("peer_addr", &self.stream.peer_addr())
            .finish()
    }
}

impl TestClient {
    /// Connect to `addr` (loopback in the tests) and prepare the read buffer.
    pub async fn connect(addr: std::net::SocketAddr) -> Result<Self, HarnessError> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let registry = Registry::embedded()?;
        Ok(Self {
            stream,
            buf: BytesMut::with_capacity(4096),
            registry,
            compression: None,
        })
    }

    /// The address of the remote end (i.e. the backend's ephemeral port).
    pub fn peer_addr(&self) -> Result<std::net::SocketAddr, HarnessError> {
        Ok(self.stream.peer_addr()?)
    }

    /// Send the handshake packet.
    pub async fn send_handshake(
        &mut self,
        protocol: i32,
        server_address: &str,
        server_port: u16,
        intent: Intent,
    ) -> Result<(), HarnessError> {
        let handshake = Handshake {
            protocol,
            server_address: server_address.to_owned(),
            server_port,
            intent,
        };
        let payload = handshake.encode_payload()?;
        self.write_frame(Packet {
            id: Handshake::PACKET_ID,
            payload,
        })
        .await
    }

    /// Send a status request; returns the raw response payload (a JSON
    /// document the tests parse and assert on).
    pub async fn status_request(&mut self, protocol: i32) -> Result<String, HarnessError> {
        let request_id = protocol_id(
            &self.registry,
            protocol,
            PacketSlot::StatusServerboundRequest,
        )?;
        let response_id = protocol_id(
            &self.registry,
            protocol,
            PacketSlot::StatusClientboundResponse,
        )?;
        self.write_frame(Packet {
            id: request_id,
            payload: Bytes::new(),
        })
        .await?;
        let packet = self.read_packet("status response").await?;
        if packet.id != response_id {
            return Err(HarnessError::UnexpectedPacketId {
                context: "status response",
                expected: response_id,
                actual: packet.id,
            });
        }
        let json = String::from_utf8(packet.payload.to_vec()).map_err(|_| {
            HarnessError::UnexpectedData("status response is not valid UTF-8".to_owned())
        })?;
        Ok(json)
    }

    /// Send a status ping with `payload`; returns the echoed payload.
    pub async fn status_ping(&mut self, protocol: i32, payload: i64) -> Result<i64, HarnessError> {
        let ping_id = protocol_id(&self.registry, protocol, PacketSlot::StatusServerboundPing)?;
        let pong_id = protocol_id(&self.registry, protocol, PacketSlot::StatusClientboundPong)?;
        self.write_frame(Packet {
            id: ping_id,
            payload: Bytes::copy_from_slice(&payload.to_be_bytes()),
        })
        .await?;
        let packet = self.read_packet("status pong").await?;
        if packet.id != pong_id {
            return Err(HarnessError::UnexpectedPacketId {
                context: "status pong",
                expected: pong_id,
                actual: packet.id,
            });
        }
        let mut bytes = [0u8; 8];
        if packet.payload.len() != 8 {
            return Err(HarnessError::UnexpectedData(format!(
                "status pong payload is {} bytes, expected 8",
                packet.payload.len()
            )));
        }
        bytes.copy_from_slice(&packet.payload);
        Ok(i64::from_be_bytes(bytes))
    }

    /// Send the login start packet.
    ///
    /// For 1.19 and newer (protocol 759+, which is every release this
    /// registry supports except ancient ones) a real client appends its
    /// player UUID after the name; the loopback backend ignores everything
    /// past the name via `parse_login_start_name`, so the trailing UUID is
    /// sent for protocol >= 759 to stay faithful to the real client shape.
    pub async fn login_start(&mut self, protocol: i32, name: &str) -> Result<(), HarnessError> {
        let id = protocol_id(&self.registry, protocol, PacketSlot::LoginServerboundStart)?;
        let mut payload = BytesMut::with_capacity(name.len() + 21);
        write_string(&mut payload, name, 16)?;
        if protocol >= 759 {
            let (msb, lsb) = uuid_from_name(name);
            write_uuid(msb, lsb, &mut payload);
        }
        self.write_frame(Packet {
            id,
            payload: payload.freeze(),
        })
        .await
    }

    /// Read and decode the login success packet sent in response to a login
    /// start. Fails on an unexpected packet id or a malformed body.
    ///
    /// A Set Compression packet arriving before the success (vanilla sends
    /// one when the server compresses the link) is consumed transparently:
    /// the client switches to the compressed frame layout, exactly like a
    /// real client, and keeps waiting for the success. The negotiated
    /// threshold is visible via [`compression_threshold`](Self::compression_threshold).
    pub async fn expect_login_success(
        &mut self,
        protocol: i32,
    ) -> Result<LoginSuccess, HarnessError> {
        let success_id = protocol_id(
            &self.registry,
            protocol,
            PacketSlot::LoginClientboundLoginSuccess,
        )?;
        let set_compression_id = protocol_id(
            &self.registry,
            protocol,
            PacketSlot::LoginClientboundSetCompression,
        )?;
        loop {
            let packet = self.read_packet("login success").await?;
            if packet.id == set_compression_id {
                self.handle_set_compression(packet.payload)?;
                continue;
            }
            if packet.id != success_id {
                return Err(HarnessError::UnexpectedPacketId {
                    context: "login success",
                    expected: success_id,
                    actual: packet.id,
                });
            }
            let mut cursor: &[u8] = &packet.payload;
            let uuid = read_uuid(&mut cursor)?;
            let name = read_string(&mut cursor, 16)?;
            let extra = Bytes::copy_from_slice(cursor);
            return Ok(LoginSuccess { uuid, name, extra });
        }
    }

    /// The compression threshold negotiated with the server, or `None` while
    /// the link is in the plain layout.
    pub fn compression_threshold(&self) -> Option<i32> {
        self.compression
            .as_ref()
            .map(|compression| compression.threshold)
    }

    /// Switch the client over to the compressed frame layout. Called when a
    /// Set Compression packet is observed; `body` is its raw payload (a
    /// VarInt threshold). A negative threshold disables compression and is a
    /// no-op for the link state.
    fn handle_set_compression(&mut self, body: Bytes) -> Result<(), HarnessError> {
        let mut cursor: &[u8] = &body;
        let threshold = decode_varint(&mut cursor)?;
        self.compression = if threshold >= 0 {
            Some(Compression::new(threshold, -1).map_err(HarnessError::from)?)
        } else {
            None
        };
        Ok(())
    }

    /// Send a raw frame. Explicit id, explicit payload bytes.
    ///
    /// Escape hatch for packets the harness does not model — e.g. play-state
    /// frames, whose ids are deliberately not tracked by the registry. The
    /// loopback backend accepts them blindly and never replies.
    pub async fn send_frame(&mut self, packet_id: i32, payload: &[u8]) -> Result<(), HarnessError> {
        self.write_frame(Packet {
            id: packet_id,
            payload: Bytes::copy_from_slice(payload),
        })
        .await
    }

    /// Send a chat message-shaped packet into the play state.
    ///
    /// `packet_id` is explicit because play-state packet ids are out of the
    /// registry's scope (by design — see its docs). The backend reads the
    /// frame and ignores it; the only possible observable effect is the
    /// connection staying open, which the tests assert.
    pub async fn send_chat_message(
        &mut self,
        packet_id: i32,
        message: &str,
    ) -> Result<(), HarnessError> {
        let mut payload = BytesMut::with_capacity(message.len() + 5);
        write_string(&mut payload, message, 256)?;
        self.send_frame(packet_id, &payload).await
    }

    /// Read the next frame; `Ok(None)` on a clean end-of-stream. Used by
    /// teardown assertions (see the integration tests).
    pub async fn next_packet(&mut self) -> Result<Option<Packet>, HarnessError> {
        self.read_frame().await
    }

    async fn read_packet(&mut self, context: &'static str) -> Result<Packet, HarnessError> {
        self.read_frame()
            .await?
            .ok_or(HarnessError::UnexpectedEof(context))
    }

    /// Write one frame in whatever layout the link is currently in (plain
    /// before Set Compression, compressed after).
    async fn write_frame(&mut self, packet: Packet) -> Result<(), HarnessError> {
        match &self.compression {
            Some(compression) => {
                wire::write_packet_compressed(&mut self.stream, packet, compression).await
            }
            None => wire::write_packet(&mut self.stream, packet).await,
        }
    }

    /// Read one frame in whatever layout the link is currently in (plain
    /// before Set Compression, compressed after).
    async fn read_frame(&mut self) -> Result<Option<Packet>, HarnessError> {
        match &self.compression {
            Some(compression) => {
                wire::read_packet_compressed(&mut self.stream, &mut self.buf, compression).await
            }
            None => wire::read_packet(&mut self.stream, &mut self.buf).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve_version;

    #[test]
    fn login_success_accessors() {
        let success = LoginSuccess {
            uuid: uuid_from_name("Steve"),
            name: "Steve".to_owned(),
            extra: Bytes::from_static(&[0x00, 0x00]),
        };
        assert!(success.is_uuid_non_zero());
        assert!(success.is_uuid_parseable());
        assert_eq!(success.name(), "Steve");
        assert_eq!(success.raw_extra().len(), 2);
        assert_eq!(success.uuid_parts(), uuid_from_name("Steve"));
    }

    #[test]
    fn version_resolution_falls_back_for_unknown_protocols() {
        let registry = Registry::embedded().expect("embedded snapshot is valid");
        // unknown protocol resolves to the latest known release
        assert_eq!(
            resolve_version(&registry, 99999).protocol,
            registry.latest().protocol
        );
        // a known protocol resolves to itself
        assert_eq!(resolve_version(&registry, 767).protocol, 767);
    }
}
