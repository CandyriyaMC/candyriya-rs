//! Response encoders: the payloads a proxy writes when answering a
//! BungeeCord subchannel request on the same channel to the same player.
//!
//! ## Wire shapes
//!
//! Every response is a `writeUTF` subchannel name followed by the arguments
//! documented in the crate-level reference table. The leading subchannel is
//! what BungeeCord actually writes (`DownstreamBridge` starts every reply
//! with `out.writeUTF("<SubChannel>")`), and it is what backends read first
//! when they match responses.
//!
//! The single exception is [`encode_forwarded_response`]: the payload a
//! server receives after a `Forward` reaches it contains the *nested*
//! channel name as its first `writeUTF`, never the string `Forward` — the
//! proxy replaces the wrapper entirely.
//!
//! ## Type notes
//!
//! - IP ports are signed `writeInt` (`i32`), matching the wiki and the
//!   upstream `readInt`.
//! - `ServerIP`'s port is an unsigned two-byte `writeUnsignedShort` (`u16`),
//!   matching the upstream `readUnsignedShort`.
//! - UUID responses carry the dashed string form as a `readUTF` string; no
//!   36-byte binary UUID is ever on the wire.
//! - `PlayerList` and `GetServers` carry a comma-space (`", "`) separated
//!   string; an empty collection is encoded as an empty string, which a
//!   strict `split(", ")` decoder reads back as one empty entry — the same
//!   ambiguity the Java implementations have.

use candyriya_messaging::{write_java_int, write_java_string, write_java_u16};

use crate::channels::{
    SUBCHANNEL_FORWARD, SUBCHANNEL_GET_PLAYER_SERVER, SUBCHANNEL_GET_SERVER,
    SUBCHANNEL_GET_SERVERS, SUBCHANNEL_IP, SUBCHANNEL_IP_OTHER, SUBCHANNEL_PLAYER_COUNT,
    SUBCHANNEL_PLAYER_LIST, SUBCHANNEL_SERVER_IP, SUBCHANNEL_UUID, SUBCHANNEL_UUID_OTHER,
};
use crate::error::BungeeMessagingError;

/// Writes one `writeUTF` argument, attaching the subchannel to any error.
fn write_response_string(
    buf: &mut Vec<u8>,
    subchannel: &str,
    value: &str,
) -> Result<(), BungeeMessagingError> {
    write_java_string(buf, value).map_err(|source| BungeeMessagingError::Encode {
        subchannel: subchannel.to_owned(),
        source,
    })
}

/// Writes the CSV payload of `PlayerList`/`GetServers` responses:
/// `writeUTF(", ".join(values))`.
fn write_csv(
    buf: &mut Vec<u8>,
    subchannel: &str,
    values: &[&str],
) -> Result<(), BungeeMessagingError> {
    let csv = values.join(", ");
    write_response_string(buf, subchannel, &csv)
}

/// Encodes the `IP` response: `writeUTF("IP")`, `writeUTF(ip)`,
/// `writeInt(port)`.
///
/// `ip` is the literal string from the socket address; for TCP peers it is
/// the host address (not a hostname unless one was configured), for Unix
/// socket peers it has the form `"unix://<path>"` with `port` set to `0`.
/// `port` is a signed 32-bit integer on the wire.
pub fn encode_ip_response(ip: &str, port: i32) -> Result<Vec<u8>, BungeeMessagingError> {
    let mut buf = Vec::with_capacity(2 + ip.len() + 8);
    write_response_string(&mut buf, SUBCHANNEL_IP, SUBCHANNEL_IP)?;
    write_response_string(&mut buf, SUBCHANNEL_IP, ip)?;
    write_java_int(&mut buf, port);
    Ok(buf)
}

/// Encodes the `IPOther` response: `writeUTF("IPOther")`,
/// `writeUTF(username)`, `writeUTF(address)`, `writeInt(port)`.
///
/// `address` follows the same socket rules as [`encode_ip_response`].
/// `port` is a signed 32-bit integer on the wire; `username` echoes the
/// argument of the request.
pub fn encode_ip_other_response(
    username: &str,
    address: &str,
    port: i32,
) -> Result<Vec<u8>, BungeeMessagingError> {
    let mut buf = Vec::with_capacity(2 + username.len() + address.len() + 12);
    write_response_string(&mut buf, SUBCHANNEL_IP_OTHER, SUBCHANNEL_IP_OTHER)?;
    write_response_string(&mut buf, SUBCHANNEL_IP_OTHER, username)?;
    write_response_string(&mut buf, SUBCHANNEL_IP_OTHER, address)?;
    write_java_int(&mut buf, port);
    Ok(buf)
}

/// Encodes the `PlayerCount` response: `writeUTF("PlayerCount")`,
/// `writeUTF(server)`, `writeInt(count)`.
///
/// `server` echoes the argument of the request — either the server name or
/// the sentinel `"ALL"` for the global count. Counts from BungeeCord are
/// absolute player totals, not deltas.
pub fn encode_player_count_response(
    server: &str,
    count: i32,
) -> Result<Vec<u8>, BungeeMessagingError> {
    let mut buf = Vec::with_capacity(2 + server.len() + 8);
    write_response_string(&mut buf, SUBCHANNEL_PLAYER_COUNT, SUBCHANNEL_PLAYER_COUNT)?;
    write_response_string(&mut buf, SUBCHANNEL_PLAYER_COUNT, server)?;
    write_java_int(&mut buf, count);
    Ok(buf)
}

/// Encodes the `PlayerList` response: `writeUTF("PlayerList")`,
/// `writeUTF(server)`, `writeUTF(", ".join(players))`.
///
/// `server` echoes the argument of the request — either the server name or
/// the sentinel `"ALL"` for the global list. An empty slice encodes an empty
/// CSV string (see the module docs for the empty-list caveat).
pub fn encode_player_list_response(
    server: &str,
    players: &[&str],
) -> Result<Vec<u8>, BungeeMessagingError> {
    let mut buf = Vec::with_capacity(2 + server.len() + 8);
    write_response_string(&mut buf, SUBCHANNEL_PLAYER_LIST, SUBCHANNEL_PLAYER_LIST)?;
    write_response_string(&mut buf, SUBCHANNEL_PLAYER_LIST, server)?;
    write_csv(&mut buf, SUBCHANNEL_PLAYER_LIST, players)?;
    Ok(buf)
}

/// Encodes the `GetPlayerServer` response: `writeUTF("GetPlayerServer")`,
/// `writeUTF(target)`, `writeUTF(server)`.
///
/// `server` is the name of the server `target` is on, or `""` when `target`
/// is unknown or has no server — matching how BungeeCord reports it — or
/// `None` to write that empty string.
pub fn encode_get_player_server_response(
    target: &str,
    server: Option<&str>,
) -> Result<Vec<u8>, BungeeMessagingError> {
    let mut buf = Vec::with_capacity(2 + target.len() + 8);
    write_response_string(
        &mut buf,
        SUBCHANNEL_GET_PLAYER_SERVER,
        SUBCHANNEL_GET_PLAYER_SERVER,
    )?;
    write_response_string(&mut buf, SUBCHANNEL_GET_PLAYER_SERVER, target)?;
    write_response_string(&mut buf, SUBCHANNEL_GET_PLAYER_SERVER, server.unwrap_or(""))?;
    Ok(buf)
}

/// Encodes the `GetServers` response: `writeUTF("GetServers")`,
/// `writeUTF(", ".join(servers))`.
///
/// The strings are the server names as defined in the proxy's config, in
/// whatever order the caller passes them. An empty slice encodes an empty
/// CSV string (see the module docs for the empty-list caveat).
pub fn encode_get_servers_response(servers: &[&str]) -> Result<Vec<u8>, BungeeMessagingError> {
    let mut buf = Vec::with_capacity(64);
    write_response_string(&mut buf, SUBCHANNEL_GET_SERVERS, SUBCHANNEL_GET_SERVERS)?;
    write_csv(&mut buf, SUBCHANNEL_GET_SERVERS, servers)?;
    Ok(buf)
}

/// Encodes the `GetServer` response: `writeUTF("GetServer")`,
/// `writeUTF(server)`.
///
/// `server` is the name of the server this message is being sent on, as
/// defined in the proxy's config.
pub fn encode_get_server_response(server: &str) -> Result<Vec<u8>, BungeeMessagingError> {
    let mut buf = Vec::with_capacity(2 + server.len() + 4);
    write_response_string(&mut buf, SUBCHANNEL_GET_SERVER, SUBCHANNEL_GET_SERVER)?;
    write_response_string(&mut buf, SUBCHANNEL_GET_SERVER, server)?;
    Ok(buf)
}

/// Encodes the `ServerIP` response: `writeUTF("ServerIP")`,
/// `writeUTF(server)`, `writeUTF(ip)`, `writeUnsignedShort(port)`.
///
/// `port` is written as a two-byte unsigned big-endian short, exactly
/// `writeUnsignedShort`; values above 65535 are not representable and the
/// type rules them out. `ip` is the literal host address of the server
/// (e.g. `"127.0.0.1"` or an IPv6 string), never a hostname unless the
/// address was configured by name.
pub fn encode_server_ip_response(
    server: &str,
    ip: &str,
    port: u16,
) -> Result<Vec<u8>, BungeeMessagingError> {
    let mut buf = Vec::with_capacity(2 + server.len() + ip.len() + 8);
    write_response_string(&mut buf, SUBCHANNEL_SERVER_IP, SUBCHANNEL_SERVER_IP)?;
    write_response_string(&mut buf, SUBCHANNEL_SERVER_IP, server)?;
    write_response_string(&mut buf, SUBCHANNEL_SERVER_IP, ip)?;
    write_java_u16(&mut buf, port);
    Ok(buf)
}

/// Encodes the `UUID` response: `writeUTF("UUID")`, `writeUTF(uuid)`.
///
/// `uuid` is the canonical dashed string form (e.g.
/// `"069a79f4-44e9-4726-a5be-fca90e38aaf5"`); the wire never carries the
/// 36-byte binary UUID. The string is passed through untouched — callers
/// are expected to produce the dashed form themselves.
pub fn encode_uuid_response(uuid: &str) -> Result<Vec<u8>, BungeeMessagingError> {
    let mut buf = Vec::with_capacity(2 + uuid.len() + 4);
    write_response_string(&mut buf, SUBCHANNEL_UUID, SUBCHANNEL_UUID)?;
    write_response_string(&mut buf, SUBCHANNEL_UUID, uuid)?;
    Ok(buf)
}

/// Encodes the `UUIDOther` response: `writeUTF("UUIDOther")`,
/// `writeUTF(username)`, `writeUTF(uuid)`.
///
/// `uuid` follows the same dashed string convention as
/// [`encode_uuid_response`]. `username` echoes the argument of the request.
pub fn encode_uuid_other_response(
    player_name: &str,
    uuid: &str,
) -> Result<Vec<u8>, BungeeMessagingError> {
    let mut buf = Vec::with_capacity(2 + player_name.len() + uuid.len() + 8);
    write_response_string(&mut buf, SUBCHANNEL_UUID_OTHER, SUBCHANNEL_UUID_OTHER)?;
    write_response_string(&mut buf, SUBCHANNEL_UUID_OTHER, player_name)?;
    write_response_string(&mut buf, SUBCHANNEL_UUID_OTHER, uuid)?;
    Ok(buf)
}

/// Encodes the payload a server receives when a forwarded message arrives:
/// `writeUTF(channel)`, `writeShort(message.len())`, `message`.
///
/// This is the shape BungeeCord sends after a `Forward`/`ForwardToPlayer`
/// reaches the destination: no `Forward` subchannel wrapper, only the nested
/// channel name as the first field. Backends read `readUTF()`, a short and
/// then that many bytes — the nested `message` is opaque to the proxy and
/// this encoder does not inspect it.
///
/// The nested payload must fit the two-byte unsigned length prefix; a
/// longer payload is rejected with [`BungeeMessagingError::ForwardPayloadTooLong`].
pub fn encode_forwarded_response(
    channel: &str,
    message: &[u8],
) -> Result<Vec<u8>, BungeeMessagingError> {
    let length =
        u16::try_from(message.len()).map_err(|_| BungeeMessagingError::ForwardPayloadTooLong {
            subchannel: SUBCHANNEL_FORWARD.to_owned(),
            len: message.len(),
            max: u16::MAX as usize,
        })?;
    let mut buf = Vec::with_capacity(2 + channel.len() + 2 + message.len());
    write_response_string(&mut buf, SUBCHANNEL_FORWARD, channel)?;
    write_java_u16(&mut buf, length);
    buf.extend_from_slice(message);
    Ok(buf)
}
