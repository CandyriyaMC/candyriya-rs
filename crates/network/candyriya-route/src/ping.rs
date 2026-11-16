//! A real Minecraft status ping against a backend, used by the health
//! checker.
//!
//! The flow is exactly what a vanilla client does to list a server:
//!
//! 1. TCP connect to the backend;
//! 2. Handshake with [`Intent::Status`] and protocol version `-1`
//!    (the idiomatic "I only want the status, whatever I speak" version);
//! 3. Status Request (serverbound packet `0x00`, empty payload);
//! 4. The backend replies with Status Response (clientbound packet `0x00`),
//!    whose single field is a Minecraft string holding a JSON document.
//!
//! We do **not** parse the JSON: for a health check, receiving a valid framed
//! status response is proof enough that the backend speaks the protocol. The
//! first [`MOTD_PREVIEW_BYTES`] of the JSON document are kept as an MOTD
//! preview for logging purposes.
//!
//! Everything runs under a single caller-provided timeout; on expiry the
//! check reports [`RouteError::PingTimeout`].

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use candyriya_protocol::{
    DEFAULT_MAX_STRING_LENGTH, Handshake, Intent, Packet, PacketCodec, read_string,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::codec::{Decoder, Encoder};

use crate::error::RouteError;

/// How many bytes of the status JSON are kept for logging.
pub const MOTD_PREVIEW_BYTES: usize = 512;

/// Serverbound Status Request packet id (status state).
const STATUS_REQUEST_PACKET_ID: i32 = 0x00;
/// Clientbound Status Response packet id (status state).
const STATUS_RESPONSE_PACKET_ID: i32 = 0x00;

/// The outcome of a successful status ping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PingOutcome {
    /// The first [`MOTD_PREVIEW_BYTES`] bytes of the status JSON, truncated
    /// on a UTF-8 character boundary. May be empty when the backend sent an
    /// empty status string (technically legal, practically never seen).
    pub(crate) motd: String,
}

/// Pings `addr` with a status handshake and waits for the status response.
///
/// The whole operation — connect, write, read — is bounded by `timeout`.
///
/// # Errors
///
/// * [`RouteError::PingTimeout`] — the operation did not finish in time;
/// * [`RouteError::Io`] — connect or socket I/O failed;
/// * [`RouteError::Protocol`] — a frame could not be encoded or decoded;
/// * [`RouteError::UnexpectedPacketId`] — the reply was not a status response;
/// * [`RouteError::InvalidStatusResponse`] — the status payload was not a
///   well-formed Minecraft string with no trailing bytes.
pub(crate) async fn status_ping(
    addr: SocketAddr,
    timeout: Duration,
) -> Result<PingOutcome, RouteError> {
    tokio::time::timeout(timeout, ping_inner(addr))
        .await
        .map_err(|_| RouteError::PingTimeout)?
}

async fn ping_inner(addr: SocketAddr) -> Result<PingOutcome, RouteError> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;

    let handshake = Handshake {
        // -1 is the protocol version vanilla clients use when they ping to
        // discover the version; the backend does not care for a status ping
        protocol: -1,
        server_address: addr.ip().to_string(),
        server_port: addr.port(),
        intent: Intent::Status,
    };

    let mut codec = PacketCodec::new();
    let mut out = BytesMut::with_capacity(64);
    codec.encode(
        Packet {
            id: Handshake::PACKET_ID,
            payload: handshake.encode_payload()?,
        },
        &mut out,
    )?;
    codec.encode(
        Packet {
            id: STATUS_REQUEST_PACKET_ID,
            payload: Bytes::new(),
        },
        &mut out,
    )?;
    stream.write_all(&out).await?;

    let mut pending = BytesMut::with_capacity(1024);
    loop {
        match codec.decode(&mut pending)? {
            Some(packet) => {
                if packet.id != STATUS_RESPONSE_PACKET_ID {
                    return Err(RouteError::UnexpectedPacketId { id: packet.id });
                }
                return parse_status_response(&packet.payload);
            }
            None => {
                let read = stream.read_buf(&mut pending).await?;
                if read == 0 {
                    // clean EOF before a full status response arrived
                    return Err(RouteError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "backend closed the connection before sending a status response",
                    )));
                }
            }
        }
    }
}

/// Validates the payload of a clientbound status response (a single
/// Minecraft string holding a JSON document) and extracts the MOTD preview.
fn parse_status_response(payload: &[u8]) -> Result<PingOutcome, RouteError> {
    let mut slice = payload;
    let json = read_string(&mut slice, DEFAULT_MAX_STRING_LENGTH)
        .map_err(|_| RouteError::InvalidStatusResponse)?;
    if !slice.is_empty() {
        // the status response carries exactly one string on the wire
        return Err(RouteError::InvalidStatusResponse);
    }

    Ok(PingOutcome {
        motd: truncate_utf8(&json, MOTD_PREVIEW_BYTES),
    })
}

/// Truncates `s` to at most `max_bytes` bytes without splitting a multi-byte
/// UTF-8 character.
fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_payload(json: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        candyriya_protocol::write_string(&mut buf, json, DEFAULT_MAX_STRING_LENGTH)
            .expect("test string fits");
        buf
    }

    #[test]
    fn parse_valid_status_response() {
        let json =
            r#"{"version":{"name":"1.21.1","protocol":767},"players":{"max":20,"online":3}}"#;
        let outcome = parse_status_response(&status_payload(json)).expect("must parse");
        assert_eq!(outcome.motd, json);
    }

    #[test]
    fn parse_rejects_trailing_bytes() {
        let mut payload = status_payload("{}");
        payload.extend_from_slice(&[0x00, 0x00]);
        assert!(matches!(
            parse_status_response(&payload),
            Err(RouteError::InvalidStatusResponse)
        ));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(matches!(
            parse_status_response(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]),
            Err(RouteError::InvalidStatusResponse)
        ));
    }

    #[test]
    fn motd_preview_is_truncated_on_char_boundary() {
        // a multi-byte char straddles the 512-byte cut
        let mut json = "a".repeat(MOTD_PREVIEW_BYTES - 1);
        json.push('€'); // 3 bytes, would straddle
        json.push_str("rest");
        let outcome = parse_status_response(&status_payload(&json)).expect("must parse");
        assert_eq!(outcome.motd.len(), MOTD_PREVIEW_BYTES - 1);
        assert!(outcome.motd.chars().all(|ch| ch == 'a'));
    }

    #[test]
    fn motd_preview_keeps_short_documents() {
        let outcome = parse_status_response(&status_payload("{\"ok\":true}")).expect("must parse");
        assert_eq!(outcome.motd, "{\"ok\":true}");
    }
}
