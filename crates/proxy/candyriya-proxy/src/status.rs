//! The status intercept: answering the server-list ping without ever
//! considering a backend.
//!
//! The server-list ping is Vivaldi-free candy for scanners and players
//! alike: a client opens with a status-intent handshake, a status request,
//! and expects a JSON document describing the network right now. There is
//! no reason to touch any backend for this, and every proxy in the wild
//! answers from memory. This module owns that exchange end to end, after
//! [`read_handshake`](crate::read_handshake) has validated the handshake
//! and reported `intent = 1`.
//!
//! # The exchange, exactly as implemented
//!
//! 1. **The next frame must be a `status_request`.** Resolved from
//!    `candyriya-protocol-registry` for the client's protocol (falling
//!    back to the newest known release when the registry has no table
//!    for it), the frame has to be the request packet with an empty
//!    payload. Anything else — a ping-shaped frame, a garbage id, a
//!    non-empty payload — closes the connection without a word back.
//!    The whole read is under a deadline: [`HANDSHAKE_TIMEOUT_CAP`]
//!    (2 s), or the configured `[advanced] handshake-timeout` when it is
//!    shorter.
//! 2. **The response body is answered from configuration, immediately.**
//!    `version.name` is `"candyriya"`; `version.protocol` echoes the
//!    client's handshake protocol verbatim (never a translation layer —
//!    cross-version support is a backend concern); `players.max` is
//!    `[show-max-players]`; `players.online` is the live session-roster
//!    count (players in the play state right now).
//!    `description` is the `[motd]` MiniMessage parsed
//!    *strictly* — the only constructs accepted are named colors, hex
//!    colors and the formatting tags — and serialized to its JSON
//!    component shape via lobsterchat. An MOTD that fails that gate means
//!    the proxy is misconfigured, and the exchange aborts with
//!    [`ProxyError::Misconfigured`] before anything is written.
//! 3. **The favicon is configuration with an embedded fallback.** If
//!    `[motd-favicon]` points at a readable PNG (size-capped, magic
//!    bytes checked), its base64 data URI is served. When the ping would
//!    otherwise pay for a file read and a base64 encode per request, a
//!    configured [`FaviconCache`] serves it from cachegirl instead.
//!    Any failure along that path — missing file, wrong magic, oversized
//!    file, broken cache — degrades to an embedded transparent 64x64 PNG;
//!    the client always gets *some* icon.
//! 4. **The ping is echoed.** The next frame must be the `ping` packet
//!    with an exactly 8-byte payload; it is echoed back verbatim as a
//!    `pong`, then the connection is finished. Anything else closes the
//!    connection without a response.
//!
//! # Timeouts
//!
//! Both reads (status request, ping) run under the same deadline:
//! `min([advanced] handshake-timeout, 2 s)`. A client that finishes its
//! handshake and then goes quiet — a drop scenario, a half-open socket —
//! dies at that deadline with [`ProxyError::Timeout`].
//!
//! # Framing note
//!
//! Like the handshake, these reads are **byte-wise**: clients routinely
//! pipeline handshake + status request + ping in one segment, and a
//! chunked read would over-read the ping into a buffer that cannot hand
//! it back. Every frame is consumed exactly; any bytes after it stay on
//! the socket (or, after the pong, the connection closes). Frames are
//! additionally capped at a hard 64 bytes — far above the largest legal
//! frame in this exchange (10 bytes), small enough that a hostile length
//! prefix cannot grow memory.
//!
//! # Errors
//!
//! [`ProxyError::BadPacket`] for wire violations, [`ProxyError::Timeout`]
//! for the deadline, [`ProxyError::Misconfigured`] for an unusable MOTD,
//! [`ProxyError::Registry`] when the embedded protocol registry fails to
//! load. Every error path closes the connection; the client never
//! receives a half-finished exchange.

use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::codec::{Decoder, Encoder};
use tracing::{debug, warn};

use candyriya_cache::{CacheManager, cache_entry};
use candyriya_config::Config;
use candyriya_plugin_api::parse_minimessage;
use candyriya_protocol::{
    DEFAULT_MAX_STRING_LENGTH, Packet, PacketCodec, ProtocolError, write_string,
};
use candyriya_protocol_registry::{ConnectionState, Direction, PacketKind, PacketSlot, Registry};

use crate::ProxyError;
use crate::accept::ProxyState;
use crate::handshake::HandshakeResult;

/// Hard cap for any frame in the status state, in ID + payload bytes.
///
/// The largest legal frame here is the ping: one (at most five) varint id
/// byte(s) and exactly 8 payload bytes. 64 bytes leaves ample room for a
/// multi-byte id varint while refusing to hold a hostile buffer.
const MAX_STATUS_FRAME: usize = 64;

/// Wall-clock budget for each read in the status exchange, in milliseconds,
/// used when the configuration does not lower it.
///
/// 2 s, per the exchange mandate; the effective deadline is
/// `min([advanced] handshake-timeout, this)`.
const HANDSHAKE_TIMEOUT_CAP: Duration = Duration::from_secs(2);

/// Largest favicon file read from disk, in bytes.
///
/// Real icons are 64x64 PNGs of a few KiB; one MiB is generous headroom
/// and a firm bound on memory for a hostile `motd-favicon` path.
const MAX_FAVICON_BYTES: u64 = 1024 * 1024;

/// Cache key for the favicon data served from cachegirl.
///
/// One favicon per process, so one key. TTL-bounded; a file edit is picked
/// up no later than the TTL.
const FAVICON_CACHE_KEY: &str = "status.favicon";

/// The `PNG` magic bytes every favicon must start with.
const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// The embedded transparent 64x64 PNG served when no configured favicon
/// is usable. 96 bytes, compiled into the binary; its counterpart shape
/// is what every server-list implementation expects.
const EMBEDDED_FAVICON_PNG: &[u8] = include_bytes!("../assets/status-fallback.png");

/// The embedded protocol registry, loaded at most once per process
/// (shared with the login pipeline).
fn embedded_registry() -> Option<&'static Registry> {
    crate::registry::embedded_registry()
}

/// Resolves the packet ids for the status exchange for `protocol`.
///
/// The client's protocol is never validated; when the registry has no
/// table for it the newest known release is used (mirroring the status
/// harness behavior). `None` from even that lookup means the data files
/// are broken, and the caller refuses to serve rather than freestyle ids.
struct StatusIds {
    response: i32,
    pong: i32,
}

fn status_ids(registry: &Registry, protocol: i32) -> StatusIds {
    let fallback = registry.latest().protocol;
    let resolve = |slot: PacketSlot, default: i32| {
        registry
            .packet_id(protocol, slot)
            .or_else(|| registry.packet_id(fallback, slot))
            .unwrap_or(default)
    };
    StatusIds {
        response: resolve(PacketSlot::StatusClientboundResponse, 0),
        pong: resolve(PacketSlot::StatusClientboundPong, 1),
    }
}

/// Release the client spoke; newest known release when unknown.
fn resolve_version(
    registry: &Registry,
    protocol: i32,
) -> &'static candyriya_protocol_registry::VersionInfo {
    registry
        .from_protocol(protocol)
        .unwrap_or_else(|| registry.latest())
}

/// The effective deadline for a single read in the status exchange.
fn read_deadline(config: &Config) -> Duration {
    let configured = Duration::from_millis(u64::from(config.advanced.handshake_timeout));
    configured.min(HANDSHAKE_TIMEOUT_CAP)
}

/// A `status_request`-or-`ping` frame accumulator with a hard byte cap.
///
/// Same byte-wise strategy as the handshake reader — pipeline-safe — with
/// a much smaller ceiling because nothing legal in this exchange exceeds
/// 10 bytes.
struct FrameRead {
    buf: BytesMut,
    codec: PacketCodec,
}

impl FrameRead {
    fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(16),
            codec: PacketCodec::new(),
        }
    }

    fn push(&mut self, byte: &[u8]) -> Result<Option<Packet>, ProxyError> {
        if self.buf.len() + byte.len() > MAX_STATUS_FRAME {
            return Err(ProxyError::BadPacket(
                "status frame exceeds the status-state size cap".to_owned(),
            ));
        }
        self.buf.extend_from_slice(byte);
        match self.codec.decode(&mut self.buf) {
            Ok(packet) => Ok(packet),
            Err(ProtocolError::PacketTooLarge) => Err(ProxyError::BadPacket(
                "status frame declares an oversized length".to_owned(),
            )),
            Err(other) => Err(ProxyError::BadPacket(other.to_string())),
        }
    }
}

/// Reads exactly one frame with byte-wise reads under `timeout`.
///
/// Premature EOF and framing failures are [`ProxyError::BadPacket`];
/// exceeding `timeout` is [`ProxyError::Timeout`].
async fn read_frame<C>(conn: &mut C, timeout: Duration) -> Result<Packet, ProxyError>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    match tokio::time::timeout(timeout, read_frame_inner(conn)).await {
        Ok(result) => result,
        Err(_) => Err(ProxyError::Timeout),
    }
}

async fn read_frame_inner<C>(conn: &mut C) -> Result<Packet, ProxyError>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = FrameRead::new();
    let mut byte = [0_u8; 1];
    loop {
        match conn.read(&mut byte).await {
            Ok(0) => {
                return Err(ProxyError::BadPacket(
                    "connection closed before a complete status frame".to_owned(),
                ));
            }
            Ok(_) => {}
            Err(error) => return Err(ProxyError::Io(error)),
        }
        if let Some(packet) = reader.push(&byte)? {
            return Ok(packet);
        }
    }
}

/// Caches the favicon in cachegirl (namespace `cache/candyriya`).
///
/// Optional: the CLI wires this when it owns a [`CacheManager`]; without
/// one, favicons are read from disk per request. TTL-bounded on purpose
/// so a `[motd-favicon]` edit is picked up without a restart.
pub struct FaviconCache {
    namespace: Arc<candyriya_cache::Namespace>,
    ttl: Duration,
}

impl std::fmt::Debug for FaviconCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaviconCache")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl FaviconCache {
    /// A cache in the core namespace of `manager` with the given entry TTL.
    ///
    /// `registry` is `cache/candyriya`, shared with any other core data;
    /// the key is private to this exchange.
    pub fn new(manager: &CacheManager, ttl: Duration) -> candyriya_cache::Result<Self> {
        Ok(Self {
            namespace: manager.core_cache()?,
            ttl,
        })
    }

    /// Returns the cached favicon bytes, if present and readable.
    ///
    /// A cache error is logged and treated as a miss: the disk read that
    /// follows is always a safe fallback and the ping must not fail
    /// because the cache is unhealthy.
    pub fn get(&self) -> Option<Vec<u8>> {
        match cache_entry::get(&self.namespace, FAVICON_CACHE_KEY) {
            Ok(Some(entry)) => Some(entry.value),
            Ok(None) => None,
            Err(error) => {
                warn!(%error, "favicon cache lookup failed; reading from disk");
                None
            }
        }
    }

    /// Stores `favicon` under the core favicon key, replacing any stale
    /// entry.
    ///
    /// A failed store is logged and ignored — serving the bytes just read
    /// is strictly better than erroring the exchange.
    pub fn put(&self, favicon: &[u8]) {
        // cachegirl keys are unique per namespace; inserting over a live
        // key would be rejected, so clear the old one first
        match cache_entry::remove(&self.namespace, FAVICON_CACHE_KEY) {
            Ok(_) => {}
            Err(error) => {
                warn!(%error, "favicon cache eviction failed; overwriting any existing entry");
            }
        }
        if let Err(error) = cache_entry::insert(
            &self.namespace,
            FAVICON_CACHE_KEY,
            favicon.to_vec(),
            self.ttl,
        ) {
            warn!(%error, "favicon cache insert failed; favicon will be re-read on the next ping");
        }
    }
}

/// The base64 data URI for the embedded fallback favicon.
fn embedded_favicon_uri() -> &'static str {
    static URI: OnceLock<String> = OnceLock::new();
    URI.get_or_init(|| data_uri(EMBEDDED_FAVICON_PNG))
}

/// Builds the `data:image/png;base64,...` URI for `png`.
fn data_uri(png: &[u8]) -> String {
    use base64::Engine;
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

/// Validates favicon bytes read from disk.
///
/// Size cap first (impossible to exceed [`MAX_FAVICON_BYTES`]), then the
/// PNG magic. No deeper decode — a well-formed-prefix icon that renders
/// oddly is a cosmetic problem, not a reason to reject the file.
fn validate_favicon_png(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.is_empty() {
        return Err("favicon file is empty");
    }
    if bytes.len() as u64 > MAX_FAVICON_BYTES {
        return Err("favicon file is larger than 1 MiB");
    }
    if !bytes.starts_with(PNG_MAGIC) {
        return Err("favicon file is not a PNG");
    }
    Ok(())
}

/// Reads and validates the configured favicon, without touching any cache.
fn read_favicon_file(path: &str) -> Result<Vec<u8>, &'static str> {
    let path = Path::new(path);
    let metadata = std::fs::metadata(path).map_err(|_| "favicon file is not readable")?;
    if !metadata.is_file() {
        return Err("favicon path is not a regular file");
    }
    if metadata.len() > MAX_FAVICON_BYTES {
        return Err("favicon file is larger than 1 MiB");
    }
    let bytes = std::fs::read(path).map_err(|_| "favicon file is not readable")?;
    validate_favicon_png(&bytes)?;
    Ok(bytes)
}

/// Resolves the favicon for the status response, as a data URI.
///
/// Order: configured cache (if any) → configured path → the embedded
/// transparent 64x64. Any failure degrades to the embedded icon; the
/// exchange is never failed over a favicon.
fn resolve_favicon(state: &ProxyState) -> String {
    let config = state.config();
    let path = config.motd_favicon.trim();
    if path.is_empty() {
        return embedded_favicon_uri().to_owned();
    }

    if let Some(cache) = &state.favicon_cache
        && let Some(bytes) = cache.get()
        && validate_favicon_png(&bytes).is_ok()
    {
        return data_uri(&bytes);
    }

    match read_favicon_file(path) {
        Ok(bytes) => {
            if let Some(cache) = &state.favicon_cache {
                cache.put(&bytes);
            }
            data_uri(&bytes)
        }
        Err(reason) => {
            debug!(
                reason,
                path, "favicon unavailable; serving the embedded icon"
            );
            embedded_favicon_uri().to_owned()
        }
    }
}

/// Parses the configured MOTD with the strict MiniMessage gate.
///
/// The strict gate is the same one used for plugin-authored strings:
/// anything lobsterchat would silently truncate is rejected instead. An
/// empty MOTD is treated as misconfiguration as well — a blank server
/// list entry is never what the operator meant.
fn motd_component(motd: &str) -> Result<serde_json::Value, ProxyError> {
    if motd.trim().is_empty() {
        return Err(ProxyError::Misconfigured(
            "the MOTD is empty; set `motd` in candyriya.toml to a MiniMessage string".to_owned(),
        ));
    }
    let component = parse_minimessage(motd).map_err(|error| {
        ProxyError::Misconfigured(format!("the MOTD is not valid MiniMessage: {error}"))
    })?;
    serde_json::to_value(component).map_err(|error| {
        ProxyError::Misconfigured(format!("failed to render the MOTD component: {error}"))
    })
}

/// Encodes the status-response JSON as the `String` payload the vanilla
/// wire format demands (a `VarInt` byte count followed by the UTF-8 bytes).
///
/// The JSON document is pure `serde_json` output, so it is valid UTF-8 by
/// construction; the only way the encode fails is the `String` cap, which
/// means the resolved favicon pushed the document past `max_len` units.
fn encode_status_payload(body: &[u8]) -> Result<Bytes, ProxyError> {
    let json = std::str::from_utf8(body).map_err(|_| {
        ProxyError::Misconfigured("the status response is not valid UTF-8".to_owned())
    })?;
    let mut payload = BytesMut::with_capacity(body.len() + 5);
    write_string(&mut payload, json, DEFAULT_MAX_STRING_LENGTH).map_err(|error| {
        ProxyError::Misconfigured(format!(
            "the status response does not fit the wire: {error}"
        ))
    })?;
    Ok(payload.freeze())
}

/// Builds the status response body, ready to be framed.
///
/// `online` is the live play-state roster count at the time of the request;
/// `description` is the already-parsed MOTD component.
fn build_status_json(
    protocol: i32,
    max_players: u32,
    online: u64,
    description: &serde_json::Value,
    favicon_uri: &str,
) -> Vec<u8> {
    let body = serde_json::json!({
        "version": {
            "name": "candyriya",
            "protocol": protocol,
        },
        "players": {
            "max": max_players,
            "online": online,
        },
        "description": description,
        "favicon": favicon_uri,
    });
    // serde_json serialization of a Value cannot fail (no maps with
    // non-string keys, no custom serializers); unwrap would still be a
    // smell, so degrade to a minimal document instead of panicking.
    serde_json::to_vec(&body).unwrap_or_else(|_| {
        format!(
            r#"{{"version":{{"name":"candyriya","protocol":{protocol}}},"players":{{"max":{max_players},"online":{online}}},"description":{{"text":""}}}}"#
        )
        .into_bytes()
    })
}

/// Encodes and writes a packet frame to `conn`.
async fn write_frame<C>(conn: &mut C, packet: Packet) -> Result<(), ProxyError>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let mut framed = BytesMut::with_capacity(512);
    PacketCodec::new()
        .encode(packet, &mut framed)
        .map_err(|error| ProxyError::BadPacket(error.to_string()))?;
    conn.write_all(&framed).await.map_err(ProxyError::Io)
}

/// Validates a frame from the status state as a packet of kind `expected`.
fn classify_status(
    registry: &Registry,
    protocol: i32,
    packet: &Packet,
    expected: PacketKind,
) -> bool {
    let version = resolve_version(registry, protocol);
    registry.classify(
        ConnectionState::Status,
        Direction::Serverbound,
        version,
        packet.id,
    ) == expected
}

/// Answers the server-list ping.
///
/// Takes over the connection after [`read_handshake`](crate::read_handshake)
/// has validated a status-intent handshake; on return the caller must
/// drop the connection. `conn` is consumed by its position: both reads
/// are bounded by the same `min([advanced] handshake-timeout, 2 s)`
/// deadline, and the function performs the whole exchange — status
/// request in, status response + pong out — without a backend.
///
/// See the module-level documentation for the exact exchange and the
/// error taxonomy. On any error the connection is closed without a
/// response.
pub async fn handle_status<C>(
    state: &Arc<ProxyState>,
    conn: &mut C,
    handshake: &HandshakeResult,
) -> Result<(), ProxyError>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let registry = embedded_registry().ok_or_else(|| {
        ProxyError::Registry("embedded protocol snapshot failed to load".to_owned())
    })?;
    let config = state.config();
    let protocol = handshake.protocol;
    let ids = status_ids(registry, protocol);
    let deadline = read_deadline(&config);

    // 1. the next frame must be a status request with an empty payload
    let request = read_frame(conn, deadline).await?;
    if !classify_status(registry, protocol, &request, PacketKind::StatusRequest) {
        return Err(ProxyError::BadPacket(format!(
            "expected the status request packet, got id {}",
            request.id
        )));
    }
    if !request.payload.is_empty() {
        return Err(ProxyError::BadPacket(
            "status request carries a non-empty payload".to_owned(),
        ));
    }

    // 2. answer immediately from configuration; no backend is contacted
    let description = motd_component(&config.motd)?;
    let favicon = resolve_favicon(state);
    // "players online" is the live play-state roster, not the raw
    // connection counter: a server-list ping is not a player
    let online = state.roster.len() as u64;
    let body = build_status_json(
        protocol,
        config.show_max_players,
        online,
        &description,
        &favicon,
    );
    let body_bytes = body.len();
    // the vanilla status response carries the JSON as a length-prefixed
    // String; raw bytes would be read as an (empty) string by the client
    let payload = match encode_status_payload(&body) {
        Ok(payload) => payload,
        Err(error) => {
            // the resolved favicon pushed the document past the String cap;
            // the embedded icon is small and always fits
            warn!(
                %error,
                "the status response is too large for the wire; serving it without the configured favicon"
            );
            let fallback = build_status_json(
                protocol,
                config.show_max_players,
                online,
                &description,
                embedded_favicon_uri(),
            );
            encode_status_payload(&fallback)?
        }
    };
    debug!(
        protocol,
        latest = registry.latest().protocol,
        online,
        bytes = body_bytes,
        "serving the status response"
    );
    write_frame(
        conn,
        Packet {
            id: ids.response,
            payload,
        },
    )
    .await?;

    // 3. the next frame must be a ping; echo it verbatim as the pong
    let ping = read_frame(conn, deadline).await?;
    if !classify_status(registry, protocol, &ping, PacketKind::StatusPing) {
        return Err(ProxyError::BadPacket(format!(
            "expected the ping packet, got id {}",
            ping.id
        )));
    }
    if ping.payload.len() != 8 {
        return Err(ProxyError::BadPacket(format!(
            "ping payload is {} bytes, expected 8",
            ping.payload.len()
        )));
    }
    write_frame(
        conn,
        Packet {
            id: ids.pong,
            payload: ping.payload,
        },
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use candyriya_config::Config;

    fn test_config() -> Config {
        let mut config = Config::default();
        // the 2 s cap only applies under the default; tests rely on it
        // staying well below the CI watchdog
        config.advanced.handshake_timeout = 5_000;
        config
    }

    #[test]
    fn read_deadline_caps_the_configured_handshake_timeout() {
        // the exchange mandate: never more than 2 s, and the configured
        // value applies when it is shorter
        let default = Config::default();
        assert_eq!(read_deadline(&default), Duration::from_secs(2));

        let mut config = Config::default();
        config.advanced.handshake_timeout = 150;
        assert_eq!(read_deadline(&config), Duration::from_millis(150));

        let mut config = Config::default();
        config.advanced.handshake_timeout = 10_000;
        assert_eq!(read_deadline(&config), Duration::from_secs(2));
    }

    #[test]
    fn embedded_favicon_is_a_real_64x64_png() {
        assert_eq!(&EMBEDDED_FAVICON_PNG[..8], PNG_MAGIC);
        // IHDR width/height, big endian, right after the magic + IHDR tag
        let width = u32::from_be_bytes(EMBEDDED_FAVICON_PNG[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(EMBEDDED_FAVICON_PNG[20..24].try_into().unwrap());
        assert_eq!((width, height), (64, 64));

        let uri = embedded_favicon_uri();
        let body = uri
            .strip_prefix("data:image/png;base64,")
            .expect("embedded favicon must be a png data URI");
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(body)
            .expect("embedded favicon base64 must decode");
        assert_eq!(decoded.as_slice(), EMBEDDED_FAVICON_PNG);
    }

    #[test]
    fn validate_favicon_png_accepts_the_embedded_icon_and_rejects_garbage() {
        assert!(validate_favicon_png(EMBEDDED_FAVICON_PNG).is_ok());
        assert!(validate_favicon_png(&[]).is_err());
        assert!(validate_favicon_png(b"not a png at all").is_err());
        let huge = vec![0_u8; (MAX_FAVICON_BYTES + 1) as usize];
        assert!(validate_favicon_png(&huge).is_err());
        // a PNG header plus some opaque bytes is all we require
        let mut sloppy = PNG_MAGIC.to_vec();
        sloppy.extend_from_slice(b"custom bytes");
        assert!(validate_favicon_png(&sloppy).is_ok());
    }

    #[test]
    fn motd_component_parses_minimessage_and_rejects_bad_input() {
        let component = motd_component("<aqua>A Candyriya network").expect("valid motd");
        let text = component.to_string();
        assert!(text.contains("A Candyriya network"));

        // unknown tags must be rejected, never silently truncated
        assert!(matches!(
            motd_component("<rainbow>Candy</rainbow>"),
            Err(ProxyError::Misconfigured(_))
        ));
        assert!(matches!(
            motd_component(""),
            Err(ProxyError::Misconfigured(_))
        ));
        assert!(matches!(
            motd_component("   "),
            Err(ProxyError::Misconfigured(_))
        ));
        // closing a color tag is not valid MiniMessage at all
        assert!(matches!(
            motd_component("</gold>oops"),
            Err(ProxyError::Misconfigured(_))
        ));
    }

    #[test]
    fn build_status_json_echoes_protocol_and_embeds_the_motd_component() {
        let description = motd_component("<red>Pi<reset>").expect("valid motd");
        let body = build_status_json(767, 42, 3, &description, "data:image/png;base64,AAAA");
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("valid json");

        assert_eq!(parsed["version"]["name"], "candyriya");
        assert_eq!(parsed["version"]["protocol"], 767);
        assert_eq!(parsed["players"]["max"], 42);
        assert_eq!(parsed["players"]["online"], 3);
        // description must be the parsed component, not the raw string
        assert_eq!(parsed["description"], description);
        assert_eq!(parsed["favicon"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn build_status_json_uses_the_echoed_protocol_verbatim() {
        // legacy probe values and future-registry numbers pass through untouched
        for protocol in [-1, 4, 767, 777, 999_999] {
            let description = motd_component("hi").expect("valid motd");
            let body = build_status_json(protocol, 100, 0, &description, "u");
            let parsed: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
            assert_eq!(parsed["version"]["protocol"], protocol);
        }
    }

    #[test]
    fn encode_status_payload_frames_the_json_as_a_length_prefixed_string() {
        // the vanilla status response is a String (VarInt byte count +
        // UTF-8 bytes); shipping the raw JSON leaves the client decoding an
        // empty string and showing nothing — pinned down here so a refactor
        // cannot silently drop the prefix
        let description = motd_component("hi").expect("valid motd");
        let body = build_status_json(767, 100, 0, &description, "u");
        let payload = encode_status_payload(&body).expect("fits the wire");

        let mut cursor: &[u8] = &payload;
        let json = candyriya_protocol::read_string(&mut cursor, DEFAULT_MAX_STRING_LENGTH)
            .expect("a valid length-prefixed string");
        assert_eq!(json.as_bytes(), &body[..]);
        assert!(cursor.is_empty(), "no trailing bytes after the string");
    }

    #[test]
    fn frame_read_rejects_an_overflowing_declared_length() {
        // 2097153 bytes declared: past the 2 MiB hard cap the codec
        // enforces, so the frame is refused as soon as the length prefix
        // decodes — no need to buffer a megabyte of anything
        let mut reader = FrameRead::new();
        for byte in [0x81_u8, 0x80, 0x80] {
            assert!(matches!(reader.push(&[byte]), Ok(None)));
        }
        assert!(matches!(
            reader.push(&[0x01]),
            Err(ProxyError::BadPacket(_))
        ));
    }

    #[test]
    fn frame_read_rejects_a_long_declared_length_before_buffering_it() {
        // a declared length of 100 is legal for the codec but far beyond
        // anything this exchange produces; buffer growth must hit the cap
        let mut reader = FrameRead::new();
        assert!(matches!(reader.push(&[0x64]), Ok(None))); // declared 100
        let mut rejected = false;
        for _ in 0..=MAX_STATUS_FRAME {
            if reader.push(&[0x00]).is_err() {
                rejected = true;
                break;
            }
        }
        assert!(rejected, "a frame growing past the cap must be rejected");
    }

    #[test]
    fn frame_read_decodes_a_status_request() {
        let mut reader = FrameRead::new();
        let packet = reader
            .push(&[0x01, 0x00])
            .expect("cap not exceeded")
            .expect("complete frame");
        assert_eq!(packet.id, 0);
        assert!(packet.payload.is_empty());
    }

    #[test]
    fn favicon_cache_roundtrip_and_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = CacheManager::new(dir.path()).expect("cache manager");
        let cache = FaviconCache::new(&manager, Duration::from_secs(60)).expect("favicon cache");

        assert!(cache.get().is_none());
        cache.put(b"first");
        cache.put(b"second"); // must overwrite, not fail on the taken key
        assert_eq!(cache.get().as_deref(), Some(b"second".as_slice()));

        manager.shutdown().expect("cache shutdown");
    }

    #[test]
    fn resolve_favicon_reads_a_configured_file_and_caches_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let icon_path = dir.path().join("icon.png");
        // a PNG-magic file, distinct from the embedded icon
        let custom_bytes: Vec<u8> = PNG_MAGIC
            .iter()
            .cloned()
            .chain(b"custom-icon-bytes".iter().cloned())
            .collect();
        std::fs::File::create(&icon_path)
            .unwrap()
            .write_all(&custom_bytes)
            .unwrap();

        let cache_root = tempfile::tempdir().expect("cache root");
        let manager = CacheManager::new(cache_root.path()).expect("cache manager");
        let favicon_cache =
            Arc::new(FaviconCache::new(&manager, Duration::from_secs(60)).expect("favicon cache"));

        let mut config = test_config();
        config.motd_favicon = icon_path.to_str().expect("utf-8 path").to_owned();
        let mut state = ProxyState::new(Arc::new(config));
        state.favicon_cache = Some(favicon_cache);
        let state = Arc::new(state);

        let first = resolve_favicon(&state);
        assert_eq!(first, data_uri(&custom_bytes));

        // swap the file on disk; the cache must keep serving the old bytes
        let swapped: Vec<u8> = PNG_MAGIC
            .iter()
            .cloned()
            .chain(b"swapped-icon-bytes".iter().cloned())
            .collect();
        std::fs::write(&icon_path, &swapped).unwrap();
        let second = resolve_favicon(&state);
        assert_eq!(second, first);

        manager.shutdown().expect("cache shutdown");
    }

    #[test]
    fn resolve_favicon_with_a_missing_file_serves_the_embedded_icon() {
        let mut config = test_config();
        config.motd_favicon = "definitely/does-not/exist-favicon.png".to_owned();
        let state = Arc::new(ProxyState::new(Arc::new(config)));

        assert_eq!(resolve_favicon(&state), embedded_favicon_uri());
    }

    #[test]
    fn resolve_favicon_with_an_empty_path_serves_the_embedded_icon() {
        let mut config = test_config();
        config.motd_favicon = String::new();
        let state = Arc::new(ProxyState::new(Arc::new(config)));

        assert_eq!(resolve_favicon(&state), embedded_favicon_uri());
    }

    #[test]
    fn resolve_favicon_with_a_garbage_file_serves_the_embedded_icon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let icon_path = dir.path().join("garbage.png");
        std::fs::write(&icon_path, b"definitely not a png").unwrap();

        let mut config = test_config();
        config.motd_favicon = icon_path.to_str().expect("utf-8 path").to_owned();
        let state = Arc::new(ProxyState::new(Arc::new(config)));

        assert_eq!(resolve_favicon(&state), embedded_favicon_uri());
    }

    #[test]
    fn status_ids_resolve_for_known_and_unknown_protocols() {
        let registry = Registry::embedded().expect("embedded snapshot");
        for protocol in [4, 767, 777, i32::MAX] {
            let ids = status_ids(&registry, protocol);
            assert_eq!(ids.response, 0);
            assert_eq!(ids.pong, 1);
        }
    }
}
