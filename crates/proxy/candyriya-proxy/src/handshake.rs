//! The handshake pipeline: reading and validating the client's first frame.
//!
//! Every Minecraft Java Edition client opens with exactly one Handshake
//! packet (packet id `0x00`) that says what it wants: the server list
//! (`intent = 1`, status), the login flow (`intent = 2`, login) or a
//! server-side transfer (`intent = 3`, transfer). This module owns that one
//! frame and nothing else: no status or login handling, no routing, no
//! writes back to the client.
//!
//! # What is enforced, in order
//!
//! 1. **A hard frame cap.** A frame whose declared length exceeds
//!    [`MAX_PACKET_SIZE`] (2 MiB) is rejected with [`ProxyError::HandshakeTooLarge`]
//!    the moment its length prefix is decoded, without reading the body;
//!    [`HandshakeRead::push`] additionally refuses to buffer bytes beyond
//!    the cap, so a hostile peer cannot grow our memory by dripping bytes.
//! 2. **Framing.** The frame is decoded with [`candyriya_protocol::PacketCodec`].
//!    Anything that fails there is [`ProxyError::BadPacket`].
//! 3. **Packet id.** The first (and only) frame must carry id `0x00`.
//! 4. **Intent.** `1`, `2` and `3` are accepted. Anything else is
//!    [`ProxyError::BadIntent`], unless [`HandshakeLimits::allow_legacy_intents`]
//!    is set, in which case the raw value is passed through as
//!    [`Intent::Unknown`] for a forwarding pipeline to deal with.
//! 5. **Protocol version.** Any `i32` is accepted, including the negative
//!    values used by legacy version probes. A version newer than the newest
//!    release this build tracks is *not* rejected — the client may speak a
//!    version released after our registry snapshot — it is logged at WARN
//!    and let through, so future clients can connect to a proxy that has
//!    not been rebuilt yet.
//! 6. **`server_address`.** At most [`HandshakeLimits::max_address_len`]
//!    UTF-16 code units (the unit the wire format counts `String(255)` in,
//!    matching the protocol crate's accounting), and every byte must be in
//!    `[a-z0-9A-Z.\-_:]`. Anything else — NUL, control bytes, path
//!    separators, `;`, non-ASCII — is [`ProxyError::IllegalAddress`]: the
//!    address ends up in logs, in forced-host matching and in the legacy
//!    Bungee forwarding payload, so it is treated as untrusted markup, not
//!    as an opaque hostname. The charset covers DNS names, IPv4, IPv6
//!    (hence `:`) and the `_` seen in some SRV setups. An empty address is
//!    allowed; vanilla clients ping with an empty host.
//!
//! `server_port` is a `u16` on the wire, so no value can wrap or exceed
//! `65535`; `0` is passed through untouched.
//!
//! # Reading strategy
//!
//! [`read_handshake`] reads **one byte at a time** off the socket. That
//! looks wasteful until the pipelined-write case is considered: clients
//! routinely send Handshake and (for login) Login Start in a single
//! segment, and a chunked `read` cannot tell where the handshake ends
//! without over-reading into the next packet's bytes, which this slice has
//! no way to hand on. Byte-wise reads guarantee that the frame is consumed
//! exactly and every byte after it stays on the socket for the login/status
//! stage. The handshake is a one-shot control message (tens of bytes), so
//! the extra syscalls are noise. Callers that already own a buffer can use
//! [`HandshakeRead`] directly to feed it in any chunking.
//!
//! The deadline is applied around the whole read, not per byte: a slow
//! drip that never completes the frame still dies at
//! [`HandshakeLimits::timeout`], mapped to [`ProxyError::Timeout`].

use std::sync::OnceLock;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio_util::codec::Decoder;
use tracing::warn;

use candyriya_config::Config;
use candyriya_protocol::{Handshake, Intent, MAX_PACKET_SIZE, Packet, PacketCodec, ProtocolError};
use candyriya_protocol_registry::Registry;

use crate::ProxyError;

/// Default cap for the `server_address` field, in UTF-16 code units.
///
/// 255, the `String(255)` the wire format assigns to the field.
pub const DEFAULT_MAX_ADDRESS_LEN: usize = 255;

/// Boundary check for the frame cap, split out so the exact edge can be
/// tested without allocating a 2 MiB buffer.
///
/// Returns `true` when buffering `incoming` more bytes on top of `buffered`
/// would exceed [`MAX_PACKET_SIZE`] — or on arithmetic overflow, which is
/// treated as exceeding.
fn frame_cap_exceeded(buffered: usize, incoming: usize) -> bool {
    buffered
        .checked_add(incoming)
        .is_none_or(|total| total > MAX_PACKET_SIZE)
}

/// `true` for the bytes an address may legally consist of.
///
/// The set is deliberately pure ASCII: anything non-ASCII is rejected
/// without further decoding, which is what keeps encoded payloads
/// (NUL, newlines, `;`, `/`, markup) out of logs and forwarding payloads.
const fn is_legal_address_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b':'
    )
}

/// Checks the hostname portion of the handshake against the length cap and
/// the byte charset. Length first: the message distinguishes the two
/// failure modes and both are [`ProxyError::IllegalAddress`].
fn validate_address(address: &str, max_address_len: usize) -> Result<(), ProxyError> {
    let units = address.chars().map(char::len_utf16).sum::<usize>();
    if units > max_address_len {
        return Err(ProxyError::IllegalAddress(format!(
            "longer than {max_address_len} UTF-16 code units ({units})"
        )));
    }
    if let Some(byte) = address.bytes().find(|b| !is_legal_address_byte(*b)) {
        return Err(ProxyError::IllegalAddress(format!(
            "contains byte 0x{byte:02X} outside [a-z0-9A-Z.\\-_:]"
        )));
    }
    Ok(())
}

/// The newest protocol version this build tracks, read from the registry's
/// embedded snapshot (26.3, protocol 777, at snapshot time) rather than a
/// hand-copied constant, so the warning moves with the data.
///
/// `None` if the embedded snapshot fails to load — impossible in
/// practice, and treated as "never warn" rather than a bogus rejection.
fn max_tracked_protocol() -> Option<i32> {
    static MAX: OnceLock<Option<i32>> = OnceLock::new();
    *MAX.get_or_init(|| {
        Registry::embedded()
            .ok()
            .map(|registry| registry.latest().protocol)
    })
}

/// Incremental state machine accumulating bytes of the first frame and
/// decoding it with [`PacketCodec`].
///
/// Feed bytes with [`push`](HandshakeRead::push) until it returns `Some`;
/// after any error the connection must be closed — the machine does not
/// attempt error recovery, because a peer that overflows the cap or sends
/// garbage has proven it cannot be trusted to frame anything correctly.
///
/// The public `buf` is the accumulator handed to the codec; it is exposed
/// (rather than hidden behind accessors) so callers that own framing can
/// inspect how many bytes are pending, and tests can construct edge
/// states directly.
#[derive(Default)]
pub struct HandshakeRead {
    /// Bytes of the frame currently being decoded, accumulated across reads.
    pub buf: BytesMut,
    codec: PacketCodec,
}

impl HandshakeRead {
    /// A machine with no bytes buffered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes buffered so far (pending frame bytes, not yet decoded).
    pub fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    /// Feeds freshly received bytes.
    ///
    /// Returns `Ok(Some(packet))` once a complete frame has decoded,
    /// `Ok(None)` when more bytes are needed, [`ProxyError::HandshakeTooLarge`]
    /// if buffering this chunk would exceed the 2 MiB cap (or the frame's
    /// declared length already exceeds it), or [`ProxyError::BadPacket`] for
    /// any other framing failure.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Option<Packet>, ProxyError> {
        if frame_cap_exceeded(self.buf.len(), chunk.len()) {
            return Err(ProxyError::HandshakeTooLarge);
        }
        self.buf.extend_from_slice(chunk);
        match self.codec.decode(&mut self.buf) {
            Ok(packet) => Ok(packet),
            Err(ProtocolError::PacketTooLarge) => Err(ProxyError::HandshakeTooLarge),
            Err(other) => Err(ProxyError::BadPacket(other.to_string())),
        }
    }
}

/// Limits and knobs for [`read_handshake`].
///
/// `timeout` maps from `[advanced] handshake-timeout` in the configuration
/// (milliseconds there, [`Duration`] here). `max_address_len` defaults to
/// [`DEFAULT_MAX_ADDRESS_LEN`]; lowering it is a hardening knob, raising it
/// is allowed but has no spec to stand on. `allow_legacy_intents` relaxes
/// the intent check from "exactly 1, 2 or 3" to "anything, passed through as
/// [`Intent::Unknown`]"; the default is strict, and vanilla clients only
/// ever send the three known values.
#[derive(Debug, Clone)]
pub struct HandshakeLimits {
    /// Wall-clock budget for the whole handshake read.
    pub timeout: Duration,
    /// Maximum `server_address` length, in UTF-16 code units.
    pub max_address_len: usize,
    /// When set, intents outside `{1, 2, 3}` are accepted and surfaced as
    /// [`Intent::Unknown`] instead of [`ProxyError::BadIntent`]. Off by
    /// default; intended for forwarding pipelines that want a backend to
    /// see (and reject) what we cannot parse.
    pub allow_legacy_intents: bool,
}

impl HandshakeLimits {
    /// Builds limits from explicit values.
    pub fn new(timeout: Duration, max_address_len: usize, allow_legacy_intents: bool) -> Self {
        Self {
            timeout,
            max_address_len,
            allow_legacy_intents,
        }
    }

    /// Limits derived from the proxy configuration: `[advanced]
    /// handshake-timeout` (milliseconds), the default 255-unit address cap,
    /// and strict intents.
    pub fn from_config(config: &Config) -> Self {
        Self {
            timeout: Duration::from_millis(u64::from(config.advanced.handshake_timeout)),
            max_address_len: DEFAULT_MAX_ADDRESS_LEN,
            allow_legacy_intents: false,
        }
    }
}

/// The validated contents of the client's handshake frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeResult {
    /// The protocol version the client plans to use. Any `i32`, including
    /// negative legacy-probe values; nothing here is version-specific
    /// beyond the future-version WARN.
    pub protocol: i32,
    /// The `server_address` the client connected to, length-capped and
    /// charset-validated by this module.
    pub server_address: String,
    /// The `server_port` the client connected to. `0` is legal on the wire
    /// and passed through.
    pub server_port: u16,
    /// What the client wants to do next; `1` status, `2` login, `3`
    /// transfer, or `Unknown(raw)` when `allow_legacy_intents` is set.
    pub intent: Intent,
}

/// Reads and validates the client's first frame.
///
/// On success the pending bytes for the *next* stage are guaranteed to
/// still be on `stream`: this function consumes exactly the handshake
/// frame (see the module docs for why the reads are byte-wise). The caller
/// is expected to route or reject the connection based on
/// [`HandshakeResult::intent`].
///
/// The whole read is wrapped in one deadline: exceeding
/// [`HandshakeLimits::timeout`] yields [`ProxyError::Timeout`], a premature
/// EOF yields [`ProxyError::BadPacket`], and an oversized or undecodable
/// frame yields [`ProxyError::HandshakeTooLarge`] / [`ProxyError::BadPacket`]
/// respectively.
pub async fn read_handshake(
    stream: &mut TcpStream,
    limits: &HandshakeLimits,
) -> Result<HandshakeResult, ProxyError> {
    match tokio::time::timeout(limits.timeout, read_first_frame(stream, limits)).await {
        Ok(result) => result,
        Err(_) => Err(ProxyError::Timeout),
    }
}

async fn read_first_frame(
    stream: &mut TcpStream,
    limits: &HandshakeLimits,
) -> Result<HandshakeResult, ProxyError> {
    let mut reader = HandshakeRead::new();
    let mut byte = [0_u8; 1];
    loop {
        match stream.read(&mut byte).await {
            Ok(0) => {
                return Err(ProxyError::BadPacket(
                    "connection closed before a complete handshake frame".to_owned(),
                ));
            }
            Ok(_) => {}
            Err(error) => return Err(ProxyError::Io(error)),
        }
        if let Some(packet) = reader.push(&byte)? {
            return validate_first_packet(packet, limits);
        }
    }
}

/// Applies the semantic checks (packet id, intent, protocol version,
/// address) to a fully decoded first frame.
fn validate_first_packet(
    packet: Packet,
    limits: &HandshakeLimits,
) -> Result<HandshakeResult, ProxyError> {
    if packet.id != Handshake::PACKET_ID {
        return Err(ProxyError::BadPacket(format!(
            "first packet id {} is not the handshake id 0",
            packet.id
        )));
    }
    let mut payload = packet.payload;
    let handshake = Handshake::decode_payload(&mut payload)
        .map_err(|error| ProxyError::BadPacket(error.to_string()))?;

    match handshake.intent {
        Intent::Status | Intent::Login | Intent::Transfer => {}
        Intent::Unknown(raw) => {
            if !limits.allow_legacy_intents {
                return Err(ProxyError::BadIntent(raw));
            }
            warn!(
                protocol = handshake.protocol,
                intent = raw,
                "accepting unknown handshake intent (allow_legacy_intents)"
            );
        }
    }

    if let Some(max) = max_tracked_protocol()
        && handshake.protocol > max
    {
        // deliberate: future clients are let through with a warning, never
        // rejected — the proxy may be older than their release
        warn!(
            protocol = handshake.protocol,
            max_tracked = max,
            "client speaks a newer protocol than this build tracks; accepting anyway"
        );
    }

    validate_address(&handshake.server_address, limits.max_address_len)?;

    Ok(HandshakeResult {
        protocol: handshake.protocol,
        server_address: handshake.server_address,
        server_port: handshake.server_port,
        intent: handshake.intent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// Test-local VarInt encoder, deliberately independent of the
    /// production one so the frames are built from first principles.
    fn varint(value: i32) -> Vec<u8> {
        let mut remaining = value as u32;
        let mut out = Vec::new();
        loop {
            let mut byte = (remaining & 0x7F) as u8;
            remaining >>= 7;
            if remaining != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if remaining == 0 {
                break;
            }
        }
        out
    }

    /// Frames `payload` after a packet id, with the length prefix.
    fn frame(id: i32, payload: &[u8]) -> Vec<u8> {
        let mut body = varint(id);
        body.extend_from_slice(payload);
        let mut out = varint(body.len() as i32);
        out.extend_from_slice(&body);
        out
    }

    /// The handshake payload: protocol, string address, port, intent.
    fn handshake_payload(protocol: i32, address: &str, port: u16, intent: i32) -> Vec<u8> {
        let mut out = varint(protocol);
        out.extend_from_slice(&varint(address.len() as i32)); // test addresses are ASCII
        out.extend_from_slice(address.as_bytes());
        out.extend_from_slice(&port.to_be_bytes());
        out.extend_from_slice(&varint(intent));
        out
    }

    fn handshake_frame(protocol: i32, address: &str, port: u16, intent: i32) -> Vec<u8> {
        frame(
            Handshake::PACKET_ID,
            &handshake_payload(protocol, address, port, intent),
        )
    }

    fn strict_limits() -> HandshakeLimits {
        HandshakeLimits::new(Duration::from_secs(5), DEFAULT_MAX_ADDRESS_LEN, false)
    }

    /// A loopback server/client pair; the server end is what
    /// `read_handshake` reads from.
    async fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (server, client)
    }

    async fn read_with_bytes(
        bytes: &[u8],
        limits: &HandshakeLimits,
    ) -> Result<HandshakeResult, ProxyError> {
        let (mut server, mut client) = loopback_pair().await;
        client.write_all(bytes).await.unwrap();
        read_handshake(&mut server, limits).await
    }

    #[test]
    fn frame_cap_boundary() {
        // exactly at the cap is fine, one byte over is not
        assert!(!frame_cap_exceeded(MAX_PACKET_SIZE, 0));
        assert!(!frame_cap_exceeded(MAX_PACKET_SIZE - 1, 1));
        assert!(frame_cap_exceeded(MAX_PACKET_SIZE, 1));
        // a single oversized chunk is caught even on an empty buffer
        assert!(frame_cap_exceeded(0, MAX_PACKET_SIZE + 1));
        assert!(!frame_cap_exceeded(0, MAX_PACKET_SIZE));
        // overflow counts as exceeding, not as wrapping around
        assert!(frame_cap_exceeded(usize::MAX, 1));
    }

    #[test]
    fn handshake_frame_status_golden_bytes() {
        // 22-byte frame: length 0x15, id 0x00, protocol 767 (FF 05),
        // String(14) "mc.example.com", port 25565 (63 DD), intent 1
        let expected: &[u8] = &[
            0x15, 0x00, 0xFF, 0x05, 0x0E, b'm', b'c', b'.', b'e', b'x', b'a', b'm', b'p', b'l',
            b'e', b'.', b'c', b'o', b'm', 0x63, 0xDD, 0x01,
        ];
        assert_eq!(handshake_frame(767, "mc.example.com", 25565, 1), expected);
    }

    #[tokio::test]
    async fn status_intent_frame_is_accepted() {
        let frame = handshake_frame(767, "mc.example.com", 25565, 1);
        let result = read_with_bytes(&frame, &strict_limits()).await.unwrap();
        assert_eq!(result.protocol, 767);
        assert_eq!(result.server_address, "mc.example.com");
        assert_eq!(result.server_port, 25565);
        assert_eq!(result.intent, Intent::Status);
    }

    #[tokio::test]
    async fn login_intent_frame_is_accepted() {
        let frame = handshake_frame(767, "play.example.net", 25565, 2);
        let result = read_with_bytes(&frame, &strict_limits()).await.unwrap();
        assert_eq!(result.intent, Intent::Login);
        assert_eq!(result.server_address, "play.example.net");
    }

    #[tokio::test]
    async fn transfer_intent_frame_is_accepted() {
        let frame = handshake_frame(767, "hub.example.net", 25565, 3);
        let result = read_with_bytes(&frame, &strict_limits()).await.unwrap();
        assert_eq!(result.intent, Intent::Transfer);
        assert_eq!(result.server_address, "hub.example.net");
    }

    #[tokio::test]
    async fn unknown_intent_is_rejected() {
        let frame = handshake_frame(767, "a.example.com", 25565, 4);
        assert!(matches!(
            read_with_bytes(&frame, &strict_limits()).await,
            Err(ProxyError::BadIntent(4))
        ));
    }

    #[tokio::test]
    async fn unknown_intent_passes_through_when_allowed() {
        let frame = handshake_frame(767, "a.example.com", 25565, 4);
        let limits = HandshakeLimits::new(Duration::from_secs(5), DEFAULT_MAX_ADDRESS_LEN, true);
        let result = read_with_bytes(&frame, &limits).await.unwrap();
        assert_eq!(result.intent, Intent::Unknown(4));
    }

    #[tokio::test]
    async fn wrong_packet_id_is_rejected() {
        let frame = frame(0x01, &handshake_payload(767, "a.example.com", 25565, 1));
        assert!(matches!(
            read_with_bytes(&frame, &strict_limits()).await,
            Err(ProxyError::BadPacket(_))
        ));
    }

    #[tokio::test]
    async fn negative_protocol_is_accepted() {
        // legacy version probes use -1; the handshake must not reject it
        let frame = handshake_frame(-1, "a.example.com", 25565, 1);
        let result = read_with_bytes(&frame, &strict_limits()).await.unwrap();
        assert_eq!(result.protocol, -1);
    }

    #[tokio::test]
    async fn newer_protocol_than_tracked_is_accepted_not_rejected() {
        // way past anything the registry tracks; must warn, not reject
        let frame = handshake_frame(i32::MAX, "a.example.com", 25565, 2);
        let result = read_with_bytes(&frame, &strict_limits()).await.unwrap();
        assert_eq!(result.protocol, i32::MAX);
    }

    #[tokio::test]
    async fn semicolon_in_address_is_rejected() {
        let frame = handshake_frame(767, "a;b.example.com", 25565, 2);
        assert!(matches!(
            read_with_bytes(&frame, &strict_limits()).await,
            Err(ProxyError::IllegalAddress(_))
        ));
    }

    #[tokio::test]
    async fn oversized_address_is_rejected() {
        let long_address = "a".repeat(256);
        let frame = handshake_frame(767, &long_address, 25565, 2);
        assert!(matches!(
            read_with_bytes(&frame, &strict_limits()).await,
            Err(ProxyError::IllegalAddress(_))
        ));
        // exactly 255 passes
        let max_address = "a".repeat(255);
        let frame = handshake_frame(767, &max_address, 25565, 2);
        assert!(read_with_bytes(&frame, &strict_limits()).await.is_ok());
    }

    #[tokio::test]
    async fn address_charset_accepts_dns_ipv4_ipv6_and_underscore() {
        for address in [
            "mc.example.com",
            "-1.-2.-3.-4",
            "::1",
            "2001:db8::ff00",
            "_a.example.com",
            "a-b_c:d.e",
            "A0.Z-9:_",
        ] {
            let frame = handshake_frame(767, address, 25565, 2);
            assert!(
                read_with_bytes(&frame, &strict_limits()).await.is_ok(),
                "address {address} should be accepted"
            );
        }
    }

    #[tokio::test]
    async fn address_with_space_and_non_ascii_is_rejected() {
        for address in [
            "a b.example.com",
            "mä.example.com",
            "a/b.example.com",
            "a@b.example.com",
            "a\0b.example.com",
        ] {
            let frame = handshake_frame(767, address, 25565, 2);
            assert!(
                matches!(
                    read_with_bytes(&frame, &strict_limits()).await,
                    Err(ProxyError::IllegalAddress(_))
                ),
                "address {address:?} should be rejected"
            );
        }
    }

    #[tokio::test]
    async fn port_zero_is_passed_through() {
        // impossible to overflow u16 on the wire; 0 stays 0
        let frame = handshake_frame(767, "a.example.com", 0, 1);
        let result = read_with_bytes(&frame, &strict_limits()).await.unwrap();
        assert_eq!(result.server_port, 0);
    }

    #[tokio::test]
    async fn frame_split_byte_at_a_time_is_reassembled() {
        let bytes = handshake_frame(767, "play.example.net", 25565, 2);
        let mut reader = HandshakeRead::new();
        let mut decoded = None;
        for byte in &bytes {
            if let Some(packet) = reader.push(&[*byte]).unwrap() {
                decoded = Some(packet);
            }
        }
        let packet = decoded.expect("a frame must decode by the last byte");
        assert_eq!(packet.id, Handshake::PACKET_ID);
        assert_eq!(reader.buffered_len(), 0);
    }

    #[tokio::test]
    async fn frame_split_across_socket_reads_is_reassembled() {
        // drip the frame over the loopback socket in odd-sized chunks
        let (mut server, mut client) = loopback_pair().await;
        let bytes = handshake_frame(767, "play.example.net", 25565, 2);
        let writes: Vec<Vec<u8>> = bytes.chunks(3).map(|chunk| chunk.to_vec()).collect();
        let writer = tokio::spawn(async move {
            for chunk in writes {
                client.write_all(&chunk).await.unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        let result = read_handshake(&mut server, &strict_limits()).await.unwrap();
        writer.await.unwrap();
        assert_eq!(result.intent, Intent::Login);
    }

    #[tokio::test]
    async fn oversized_declared_length_is_rejected_without_waiting_for_the_body() {
        // declared length 2097153 (MAX_PACKET_SIZE + 1); the client then
        // parks with the write half open. The error must surface as soon as
        // the length prefix decodes, not at the 10s deadline.
        let (mut server, mut client) = loopback_pair().await;
        client.write_all(&[0x81, 0x80, 0x80, 0x01]).await.unwrap();
        let limits = HandshakeLimits::new(Duration::from_secs(10), DEFAULT_MAX_ADDRESS_LEN, false);
        let result = read_handshake(&mut server, &limits).await;
        assert!(matches!(result, Err(ProxyError::HandshakeTooLarge)));
    }

    #[tokio::test]
    async fn timeout_fires_when_client_sends_nothing() {
        let (mut server, _client) = loopback_pair().await;
        let limits =
            HandshakeLimits::new(Duration::from_millis(50), DEFAULT_MAX_ADDRESS_LEN, false);
        let started = std::time::Instant::now();
        let result = read_handshake(&mut server, &limits).await;
        assert!(matches!(result, Err(ProxyError::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn eof_before_a_complete_frame_is_a_bad_packet() {
        let (mut server, client) = loopback_pair().await;
        // connect, send nothing, close
        drop(client);
        let result = read_handshake(&mut server, &strict_limits()).await;
        assert!(matches!(result, Err(ProxyError::BadPacket(_))));
    }

    #[tokio::test]
    async fn pipelined_bytes_after_the_frame_are_left_on_the_socket() {
        // a client that sends handshake + login start in one segment must
        // not lose the login start to the handshake reader
        let (mut server, mut client) = loopback_pair().await;
        let mut bytes = handshake_frame(767, "play.example.net", 25565, 2);
        bytes.extend_from_slice(b"\x10PIPELINED");
        client.write_all(&bytes).await.unwrap();
        let result = read_handshake(&mut server, &strict_limits()).await.unwrap();
        assert_eq!(result.intent, Intent::Login);
        let mut rest = [0_u8; 10];
        // "\x10PIPELINED" is 10 bytes: 1 tag + 9 letters
        server.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"\x10PIPELINED");
    }

    #[test]
    fn empty_address_is_allowed() {
        // vanilla pings with an empty host; the charset accepts the empty string
        assert!(validate_address("", DEFAULT_MAX_ADDRESS_LEN).is_ok());
    }

    #[test]
    fn address_length_counts_utf16_code_units_not_bytes() {
        // U+1D11E is two UTF-16 code units; 128 of them are 256 units > 255
        let address: String = std::iter::repeat_n('\u{1D11E}', 128).collect();
        assert!(matches!(
            validate_address(&address, DEFAULT_MAX_ADDRESS_LEN),
            Err(ProxyError::IllegalAddress(_))
        ));
        // 127 of them are 254 units, inside the cap (charset rejects these separately)
        let address: String = std::iter::repeat_n('\u{1D11E}', 127).collect();
        assert!(matches!(
            validate_address(&address, DEFAULT_MAX_ADDRESS_LEN),
            Err(ProxyError::IllegalAddress(_))
        ));
    }

    #[test]
    fn address_cap_is_configurable() {
        assert!(validate_address("abcdefghij", 10).is_ok());
        assert!(matches!(
            validate_address("abcdefghijk", 10),
            Err(ProxyError::IllegalAddress(_))
        ));
    }

    #[test]
    fn push_overflowing_declared_length_maps_to_too_large() {
        // 0x0F FF FF FF declares 268435455 (> 2 MiB)
        let mut reader = HandshakeRead::new();
        assert!(matches!(
            reader.push(&[0xFF, 0xFF, 0xFF, 0x0F]),
            Err(ProxyError::HandshakeTooLarge)
        ));
    }

    #[test]
    fn push_reports_partial_frame_as_pending() {
        let bytes = handshake_frame(767, "a.example.com", 25565, 1);
        let mut reader = HandshakeRead::new();
        for byte in &bytes[..bytes.len() - 1] {
            assert!(reader.push(&[*byte]).unwrap().is_none());
        }
        assert!(reader.buffered_len() > 0);
        assert!(reader.push(&[bytes[bytes.len() - 1]]).unwrap().is_some());
    }
}
