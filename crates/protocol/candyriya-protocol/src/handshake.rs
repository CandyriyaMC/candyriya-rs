//! The Handshake packet, the only structured packet this crate understands.
//!
//! The Handshake is sent by the client right after opening the TCP connection,
//! before anything else. Its payload is:
//!
//! 1. **Protocol Version** — a [`VarInt`](crate::decode_varint) (negative
//!    values are legal; e.g. `-1` is used by clients pinging to discover the
//!    version);
//! 2. **Server Address** — a [`String`](crate::read_string). The wiki types
//!    this field as `String(255)` for the vanilla path; this crate uses the
//!    general [`DEFAULT_MAX_STRING_LENGTH`] cap instead, because proxies can
//!    (and do) tunnel arbitrary metadata through the address field;
//! 3. **Server Port** — an unsigned 16-bit big-endian integer;
//! 4. **Intent** — a [`VarInt`](crate::decode_varint); see [`Intent`].
//!
//! The packet id on the wire is [`Handshake::PACKET_ID`]; it is not part of
//! the payload. Use [`crate::PacketCodec`] to frame the packet.
//!
//! See <https://minecraft.wiki/w/Java_Edition_protocol/Packets#Handshake>.

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::error::ProtocolError;
use crate::string::{DEFAULT_MAX_STRING_LENGTH, read_string, write_string};
use crate::varint::{decode_varint, write_varint};

/// The intent carried by a Handshake packet: what happens after it.
///
/// Values other than `1`, `2` and `3` are preserved verbatim as
/// [`Intent::Unknown`] rather than rejected, so forwarding proxies can
/// round-trip packets they do not understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intent {
    /// State 1: the client wants the server list (ping).
    Status,
    /// State 2: the client wants to log in.
    Login,
    /// State 3: the client is connecting because of a transfer packet from another server.
    ///
    /// Like state 2, this transitions to the login state, but signals that
    /// the connection is part of a server-side transfer.
    Transfer,
    /// Any other value, kept raw.
    Unknown(i32),
}

impl Intent {
    /// Builds an intent from the raw `VarInt` value on the wire.
    pub const fn from_raw(raw: i32) -> Self {
        match raw {
            1 => Self::Status,
            2 => Self::Login,
            3 => Self::Transfer,
            other => Self::Unknown(other),
        }
    }

    /// The raw `VarInt` value this intent encodes on the wire.
    pub const fn as_raw(&self) -> i32 {
        match self {
            Self::Status => 1,
            Self::Login => 2,
            Self::Transfer => 3,
            Self::Unknown(raw) => *raw,
        }
    }
}

/// A serverbound Handshake packet.
///
/// Construct it, then either serialize the payload manually with
/// [`write_payload`](Handshake::write_payload) or into a [`Bytes`] buffer with
/// [`encode_payload`](Handshake::encode_payload), and frame it with
/// [`crate::PacketCodec`] as [`crate::Packet`] with [`Handshake::PACKET_ID`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// The protocol version the client plans to use. Negative values are legal.
    pub protocol: i32,
    /// The server address the client connected to (hostname or IP).
    pub server_address: String,
    /// The server port the client connected to.
    pub server_port: u16,
    /// What the client wants to do after the handshake.
    pub intent: Intent,
}

impl Handshake {
    /// The packet id of the Handshake in the handshaking state.
    pub const PACKET_ID: i32 = 0x00;

    /// Serializes the payload (everything after the packet id) into `dst`.
    ///
    /// Returns [`ProtocolError::StringTooLong`] if `server_address` exceeds
    /// the string cap (see [`DEFAULT_MAX_STRING_LENGTH`]).
    pub fn write_payload(&self, dst: &mut impl BufMut) -> Result<(), ProtocolError> {
        write_varint(self.protocol, dst);
        write_string(dst, &self.server_address, DEFAULT_MAX_STRING_LENGTH)?;
        dst.put_u16(self.server_port);
        write_varint(self.intent.as_raw(), dst);
        Ok(())
    }

    /// Serializes the payload into a fresh [`Bytes`] buffer.
    ///
    /// Equivalent to [`write_payload`](Handshake::write_payload) followed by
    /// freezing the buffer; useful to build the `payload` of a [`crate::Packet`].
    pub fn encode_payload(&self) -> Result<Bytes, ProtocolError> {
        // rough upper bound: 5-byte protocol varint + 5-byte string prefix + address + 2 port + 5-byte intent varint
        let mut buffer = BytesMut::with_capacity(self.server_address.len() + 17);
        self.write_payload(&mut buffer)?;
        Ok(buffer.freeze())
    }

    /// Reads a payload produced by a Minecraft client from `src`.
    ///
    /// On success `src` is advanced past the exact end of the payload; any
    /// leftover bytes are reported as [`ProtocolError::TrailingData`]. The
    /// payload is validated structurally only — the packet id is not part of
    /// it and any protocol version is accepted (including negative ones).
    pub fn decode_payload(src: &mut impl Buf) -> Result<Self, ProtocolError> {
        let protocol = decode_varint(src)?;
        let server_address = read_string(src, DEFAULT_MAX_STRING_LENGTH)?;
        if src.remaining() < 2 {
            return Err(ProtocolError::Truncated);
        }
        let server_port = src.get_u16();
        let intent = Intent::from_raw(decode_varint(src)?);
        if src.remaining() != 0 {
            return Err(ProtocolError::TrailingData);
        }
        Ok(Self {
            protocol,
            server_address,
            server_port,
            intent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(handshake: &Handshake) -> Handshake {
        let payload = handshake.encode_payload().unwrap();
        let mut slice: &[u8] = &payload;
        Handshake::decode_payload(&mut slice).unwrap()
    }

    #[test]
    fn handshake_roundtrip() {
        let handshake = Handshake {
            protocol: 767,
            server_address: "mc.example.com".to_owned(),
            server_port: 25565,
            intent: Intent::Login,
        };
        assert_eq!(roundtrip(&handshake), handshake);
    }

    #[test]
    fn negative_protocol_versions_roundtrip() {
        for protocol in [-1, -2147483648, i32::MAX] {
            let handshake = Handshake {
                protocol,
                server_address: "a".to_owned(),
                server_port: 1,
                intent: Intent::Status,
            };
            assert_eq!(roundtrip(&handshake).protocol, protocol);
        }
    }

    #[test]
    fn handshake_golden_bytes() {
        let handshake = Handshake {
            protocol: -1,
            server_address: "localhost".to_owned(),
            server_port: 25565,
            intent: Intent::Login,
        };
        // FF FF FF FF 0F | 09 "localhost" | 63 DD | 02
        let expected = [
            0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0x09, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's',
            b't', 0x63, 0xDD, 0x02,
        ];
        assert_eq!(handshake.encode_payload().unwrap().as_ref(), expected);

        let mut slice: &[u8] = &expected;
        let decoded = Handshake::decode_payload(&mut slice).unwrap();
        assert_eq!(decoded, handshake);
    }

    #[test]
    fn unusual_ports_roundtrip() {
        for port in [1, 25565, 65535, 0] {
            let handshake = Handshake {
                protocol: 767,
                server_address: "x".to_owned(),
                server_port: port,
                intent: Intent::Status,
            };
            assert_eq!(roundtrip(&handshake).server_port, port);
        }
    }

    #[test]
    fn intent_variants_roundtrip() {
        for raw in [1, 2, 3, 0, 7, -1] {
            let handshake = Handshake {
                protocol: 767,
                server_address: "x".to_owned(),
                server_port: 25565,
                intent: Intent::from_raw(raw),
            };
            let decoded = roundtrip(&handshake);
            assert_eq!(decoded.intent.as_raw(), raw);
        }
        assert_eq!(Intent::from_raw(1), Intent::Status);
        assert_eq!(Intent::from_raw(2), Intent::Login);
        assert_eq!(Intent::from_raw(3), Intent::Transfer);
        assert_eq!(Intent::from_raw(4), Intent::Unknown(4));
        assert_eq!(Intent::Transfer.as_raw(), 3);
    }

    #[test]
    fn unicode_address_roundtrip() {
        let handshake = Handshake {
            protocol: 767,
            server_address: "мinecraft.example €".to_owned(),
            server_port: 25565,
            intent: Intent::Transfer,
        };
        assert_eq!(roundtrip(&handshake), handshake);
    }

    #[test]
    fn truncated_payloads_are_rejected() {
        // cut right after the protocol varint
        let mut slice: &[u8] = &[0xFF];
        assert!(matches!(
            Handshake::decode_payload(&mut slice),
            Err(ProtocolError::Truncated) | Err(ProtocolError::VarIntTooBig)
        ));
        // full handshake minus the last byte
        let handshake = Handshake {
            protocol: 767,
            server_address: "example.com".to_owned(),
            server_port: 25565,
            intent: Intent::Login,
        };
        let payload = handshake.encode_payload().unwrap();
        let mut slice: &[u8] = &payload[..payload.len() - 1];
        assert!(matches!(
            Handshake::decode_payload(&mut slice),
            Err(ProtocolError::Truncated)
        ));
        // nothing at all
        let mut slice: &[u8] = &[];
        assert!(matches!(
            Handshake::decode_payload(&mut slice),
            Err(ProtocolError::Truncated)
        ));
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut buffer = bytes::BytesMut::new();
        Handshake {
            protocol: 767,
            server_address: "example.com".to_owned(),
            server_port: 25565,
            intent: Intent::Login,
        }
        .write_payload(&mut buffer)
        .unwrap();
        buffer.extend_from_slice(&[0x00, 0x00]);
        let mut slice: &[u8] = &buffer;
        assert!(matches!(
            Handshake::decode_payload(&mut slice),
            Err(ProtocolError::TrailingData)
        ));
    }
}
