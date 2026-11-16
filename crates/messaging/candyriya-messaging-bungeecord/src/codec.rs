//! [`BungeeCodec`]: the full-payload parser and encoder.
//!
//! The wire format is a sequence of `DataOutputStream` fields: every
//! payload starts with the subchannel as a `writeUTF` string, followed by
//! the arguments of that subchannel in declaration order. `Forward` and
//! `ForwardToPlayer` carry their nested payload after a two-byte unsigned
//! length prefix.
//!
//! Parsing is **strict**: a well-formed payload is exactly consumed, and
//! any leftover byte is an error ([`BungeeMessagingError::TrailingBytes`]),
//! never silently dropped. Decoding is total on arbitrary input.

use candyriya_messaging::{
    MAX_UTF_BYTES, read_java_string, read_java_u16, write_java_string, write_java_u16,
};

use crate::channels::{
    SUBCHANNEL_CONNECT, SUBCHANNEL_CONNECT_OTHER, SUBCHANNEL_FORWARD, SUBCHANNEL_FORWARD_TO_PLAYER,
    SUBCHANNEL_GET_PLAYER_SERVER, SUBCHANNEL_GET_SERVER, SUBCHANNEL_GET_SERVERS, SUBCHANNEL_IP,
    SUBCHANNEL_IP_OTHER, SUBCHANNEL_KICK_PLAYER, SUBCHANNEL_KICK_PLAYER_RAW, SUBCHANNEL_MESSAGE,
    SUBCHANNEL_MESSAGE_RAW, SUBCHANNEL_PLAYER_COUNT, SUBCHANNEL_PLAYER_LIST, SUBCHANNEL_SERVER_IP,
    SUBCHANNEL_UUID, SUBCHANNEL_UUID_OTHER,
};
use crate::error::BungeeMessagingError;
use crate::message::BungeeMessage;

/// The longest subchannel the protocol defines, in bytes.
///
/// Only used to bound the allocation of the subchannel string on decode;
/// the longest defined subchannel is `ForwardToPlayer` (16 bytes).
const MAX_SUBCHANNEL_BYTES: usize = 64;

/// The BungeeCord plugin-messaging subchannel codec.
///
/// A zero-sized handle — `parse` and `encode` hold no state — so it is
/// cheap to construct anywhere. It is deliberately the only entry point for
/// full payloads: responses use the free functions in [`crate::response`].
///
/// # Encode errors
///
/// Encoding is fallible because the wire format is: Java `writeUTF` strings
/// cap at 65535 payload bytes and the nested `Forward` payload rides a
/// two-byte unsigned length prefix. Callers that always originate messages
/// locally can treat [`BungeeMessagingError`] as a programming error
/// (over-sized field), never as a runtime condition to design around.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BungeeCodec;

impl BungeeCodec {
    /// Parses a full plugin-message payload, reading the leading
    /// `writeUTF(subchannel)` first.
    ///
    /// See the module docs for the strictness rules. Errors carry the
    /// subchannel when one was read.
    pub fn parse(&self, data: &[u8]) -> Result<BungeeMessage, BungeeMessagingError> {
        let mut cursor = data;
        let subchannel = read_java_string(&mut cursor, MAX_SUBCHANNEL_BYTES)
            .map_err(|source| BungeeMessagingError::SubchannelPrefix { source })?;
        let message = Self::parse_arguments(&subchannel, &mut cursor)?;

        if !cursor.is_empty() {
            return Err(BungeeMessagingError::TrailingBytes {
                subchannel,
                trailing: cursor.len(),
            });
        }
        Ok(message)
    }

    /// Decodes the arguments of `subchannel` from `cursor`.
    ///
    /// Split out from [`parse`](Self::parse) so the borrow of the subchannel
    /// string is released before the trailing-bytes check.
    fn parse_arguments(
        subchannel: &str,
        cursor: &mut &[u8],
    ) -> Result<BungeeMessage, BungeeMessagingError> {
        let message = match subchannel {
            SUBCHANNEL_CONNECT => BungeeMessage::Connect {
                server: decode_string(cursor, subchannel)?,
            },
            SUBCHANNEL_CONNECT_OTHER => BungeeMessage::ConnectOther {
                target: decode_string(cursor, subchannel)?,
                server: decode_string(cursor, subchannel)?,
            },
            SUBCHANNEL_IP => BungeeMessage::IP,
            SUBCHANNEL_IP_OTHER => BungeeMessage::IPOther {
                target: decode_string(cursor, subchannel)?,
            },
            SUBCHANNEL_PLAYER_COUNT => BungeeMessage::PlayerCount {
                server: decode_string(cursor, subchannel)?,
            },
            SUBCHANNEL_PLAYER_LIST => BungeeMessage::PlayerList {
                server: decode_string(cursor, subchannel)?,
            },
            SUBCHANNEL_GET_PLAYER_SERVER => BungeeMessage::GetPlayerServer {
                target: decode_string(cursor, subchannel)?,
            },
            SUBCHANNEL_GET_SERVERS => BungeeMessage::GetServers,
            SUBCHANNEL_MESSAGE => BungeeMessage::Message {
                target: decode_string(cursor, subchannel)?,
                message: decode_string(cursor, subchannel)?,
            },
            SUBCHANNEL_MESSAGE_RAW => BungeeMessage::MessageRaw {
                target: decode_string(cursor, subchannel)?,
                message: decode_string(cursor, subchannel)?,
            },
            SUBCHANNEL_GET_SERVER => BungeeMessage::GetServer,
            SUBCHANNEL_FORWARD => {
                let (target, channel, message) = Self::parse_forward_args(cursor, subchannel)?;
                BungeeMessage::Forward {
                    target,
                    channel,
                    message,
                }
            }
            SUBCHANNEL_FORWARD_TO_PLAYER => {
                let (target, channel, message) = Self::parse_forward_args(cursor, subchannel)?;
                BungeeMessage::ForwardToPlayer {
                    target,
                    channel,
                    message,
                }
            }
            SUBCHANNEL_UUID => BungeeMessage::UUID,
            SUBCHANNEL_UUID_OTHER => BungeeMessage::UUIDOther {
                target: decode_string(cursor, subchannel)?,
            },
            SUBCHANNEL_SERVER_IP => BungeeMessage::ServerIP {
                server: decode_string(cursor, subchannel)?,
            },
            SUBCHANNEL_KICK_PLAYER => BungeeMessage::KickPlayer {
                target: decode_string(cursor, subchannel)?,
                reason: decode_string(cursor, subchannel)?,
            },
            SUBCHANNEL_KICK_PLAYER_RAW => BungeeMessage::KickPlayerRaw {
                target: decode_string(cursor, subchannel)?,
                reason: decode_string(cursor, subchannel)?,
            },
            _ => {
                return Err(BungeeMessagingError::UnknownSubchannel {
                    subchannel: subchannel.to_owned(),
                });
            }
        };
        Ok(message)
    }

    /// Parses the `Forward`/`ForwardToPlayer` argument shape, which is
    /// identical on the wire: target, nested channel, unsigned 16-bit
    /// length prefix, nested payload bytes.
    fn parse_forward_args(
        cursor: &mut &[u8],
        subchannel: &str,
    ) -> Result<(String, String, Vec<u8>), BungeeMessagingError> {
        let target = decode_string(cursor, subchannel)?;
        let channel = decode_string(cursor, subchannel)?;
        let length = decode_u16(cursor, subchannel)? as usize;
        let message = take_bytes(cursor, length, subchannel)?;
        Ok((target, channel, message))
    }

    /// Encodes a message into its full on-wire payload.
    ///
    /// Roundtrips exactly: `parse(&encode(msg)?) == Ok(msg)` for every
    /// message `Ok(encode(msg))` can produce.
    pub fn encode(&self, msg: &BungeeMessage) -> Result<Vec<u8>, BungeeMessagingError> {
        let capacity = match msg {
            BungeeMessage::Forward { message, .. }
            | BungeeMessage::ForwardToPlayer { message, .. } => 64 + message.len(),
            _ => 64,
        };
        let mut buf = Vec::with_capacity(capacity);
        match msg {
            BungeeMessage::Connect { server } => {
                write_subchannel(&mut buf, SUBCHANNEL_CONNECT);
                encode_string(&mut buf, SUBCHANNEL_CONNECT, server)?;
            }
            BungeeMessage::ConnectOther { target, server } => {
                write_subchannel(&mut buf, SUBCHANNEL_CONNECT_OTHER);
                encode_string(&mut buf, SUBCHANNEL_CONNECT_OTHER, target)?;
                encode_string(&mut buf, SUBCHANNEL_CONNECT_OTHER, server)?;
            }
            BungeeMessage::IP => write_subchannel(&mut buf, SUBCHANNEL_IP),
            BungeeMessage::IPOther { target } => {
                write_subchannel(&mut buf, SUBCHANNEL_IP_OTHER);
                encode_string(&mut buf, SUBCHANNEL_IP_OTHER, target)?;
            }
            BungeeMessage::PlayerCount { server } => {
                write_subchannel(&mut buf, SUBCHANNEL_PLAYER_COUNT);
                encode_string(&mut buf, SUBCHANNEL_PLAYER_COUNT, server)?;
            }
            BungeeMessage::PlayerList { server } => {
                write_subchannel(&mut buf, SUBCHANNEL_PLAYER_LIST);
                encode_string(&mut buf, SUBCHANNEL_PLAYER_LIST, server)?;
            }
            BungeeMessage::GetPlayerServer { target } => {
                write_subchannel(&mut buf, SUBCHANNEL_GET_PLAYER_SERVER);
                encode_string(&mut buf, SUBCHANNEL_GET_PLAYER_SERVER, target)?;
            }
            BungeeMessage::GetServers => write_subchannel(&mut buf, SUBCHANNEL_GET_SERVERS),
            BungeeMessage::Message { target, message } => {
                write_subchannel(&mut buf, SUBCHANNEL_MESSAGE);
                encode_string(&mut buf, SUBCHANNEL_MESSAGE, target)?;
                encode_string(&mut buf, SUBCHANNEL_MESSAGE, message)?;
            }
            BungeeMessage::MessageRaw { target, message } => {
                write_subchannel(&mut buf, SUBCHANNEL_MESSAGE_RAW);
                encode_string(&mut buf, SUBCHANNEL_MESSAGE_RAW, target)?;
                encode_string(&mut buf, SUBCHANNEL_MESSAGE_RAW, message)?;
            }
            BungeeMessage::GetServer => write_subchannel(&mut buf, SUBCHANNEL_GET_SERVER),
            BungeeMessage::Forward {
                target,
                channel,
                message,
            } => {
                encode_forward(&mut buf, SUBCHANNEL_FORWARD, target, channel, message)?;
            }
            BungeeMessage::ForwardToPlayer {
                target,
                channel,
                message,
            } => {
                encode_forward(
                    &mut buf,
                    SUBCHANNEL_FORWARD_TO_PLAYER,
                    target,
                    channel,
                    message,
                )?;
            }
            BungeeMessage::UUID => write_subchannel(&mut buf, SUBCHANNEL_UUID),
            BungeeMessage::UUIDOther { target } => {
                write_subchannel(&mut buf, SUBCHANNEL_UUID_OTHER);
                encode_string(&mut buf, SUBCHANNEL_UUID_OTHER, target)?;
            }
            BungeeMessage::ServerIP { server } => {
                write_subchannel(&mut buf, SUBCHANNEL_SERVER_IP);
                encode_string(&mut buf, SUBCHANNEL_SERVER_IP, server)?;
            }
            BungeeMessage::KickPlayer { target, reason } => {
                write_subchannel(&mut buf, SUBCHANNEL_KICK_PLAYER);
                encode_string(&mut buf, SUBCHANNEL_KICK_PLAYER, target)?;
                encode_string(&mut buf, SUBCHANNEL_KICK_PLAYER, reason)?;
            }
            BungeeMessage::KickPlayerRaw { target, reason } => {
                write_subchannel(&mut buf, SUBCHANNEL_KICK_PLAYER_RAW);
                encode_string(&mut buf, SUBCHANNEL_KICK_PLAYER_RAW, target)?;
                encode_string(&mut buf, SUBCHANNEL_KICK_PLAYER_RAW, reason)?;
            }
        }
        Ok(buf)
    }
}

/// Writes `subchannel` as a `writeUTF` string. Infallible: all subchannel
/// names are short ASCII constants.
fn write_subchannel(buf: &mut Vec<u8>, subchannel: &str) {
    // every subchannel constant fits the u16 length prefix by a wide margin
    write_java_string(buf, subchannel).expect("subchannel constants fit the writeUTF length prefix")
}

/// Decodes one `writeUTF` argument, attaching the subchannel to any error.
fn decode_string(cursor: &mut &[u8], subchannel: &str) -> Result<String, BungeeMessagingError> {
    read_java_string(cursor, MAX_UTF_BYTES).map_err(|source| BungeeMessagingError::Decode {
        subchannel: subchannel.to_owned(),
        source,
    })
}

/// Decodes a two-byte unsigned length argument (the nested payload length of
/// `Forward`/`ForwardToPlayer`).
fn decode_u16(cursor: &mut &[u8], subchannel: &str) -> Result<u16, BungeeMessagingError> {
    read_java_u16(cursor).map_err(|source| BungeeMessagingError::Decode {
        subchannel: subchannel.to_owned(),
        source,
    })
}

/// Takes exactly `len` bytes out of `cursor`.
fn take_bytes(
    cursor: &mut &[u8],
    len: usize,
    subchannel: &str,
) -> Result<Vec<u8>, BungeeMessagingError> {
    if cursor.len() < len {
        return Err(BungeeMessagingError::Decode {
            subchannel: subchannel.to_owned(),
            source: candyriya_messaging::JavaCodecError::Truncated {
                needed: len,
                remaining: cursor.len(),
            },
        });
    }
    let (head, rest) = cursor.split_at(len);
    *cursor = rest;
    Ok(head.to_vec())
}

/// Encodes one `writeUTF` argument, attaching the subchannel to any error.
fn encode_string(
    buf: &mut Vec<u8>,
    subchannel: &str,
    value: &str,
) -> Result<(), BungeeMessagingError> {
    write_java_string(buf, value).map_err(|source| BungeeMessagingError::Encode {
        subchannel: subchannel.to_owned(),
        source,
    })
}

/// Encodes the `Forward`/`ForwardToPlayer` shape: subchannel, target, nested
/// channel, unsigned 16-bit length prefix, nested payload bytes.
fn encode_forward(
    buf: &mut Vec<u8>,
    subchannel: &str,
    target: &str,
    channel: &str,
    message: &[u8],
) -> Result<(), BungeeMessagingError> {
    write_subchannel(buf, subchannel);
    encode_string(buf, subchannel, target)?;
    encode_string(buf, subchannel, channel)?;
    let length =
        u16::try_from(message.len()).map_err(|_| BungeeMessagingError::ForwardPayloadTooLong {
            subchannel: subchannel.to_owned(),
            len: message.len(),
            max: u16::MAX as usize,
        })?;
    write_java_u16(buf, length);
    buf.extend_from_slice(message);
    Ok(())
}
