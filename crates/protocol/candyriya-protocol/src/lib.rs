//! Pure wire-format primitives of the Minecraft Java Edition protocol.
//!
//! This crate is data-in/data-out: it knows how to read and write the
//! low-level encodings the protocol is made of, nothing more. There is no
//! async network code, no version tables (different protocol versions use
//! different packet ids and this crate deliberately stays agnostic), and no
//! packet types beyond what the framing needs. All types are `#![forbid(unsafe_code)]`.
//!
//! * [`decode_varint`] / [`decode_varlong`] / [`write_varint`] / [`write_varlong`]
//!   for the variable-length integers;
//! * [`read_string`] / [`write_string`] for length-prefixed UTF-8 strings;
//! * [`read_uuid`] / [`write_uuid`] for UUIDs in the two-longs format;
//! * [`Packet`] and [`PacketCodec`] (a `tokio_util::codec::{Decoder, Encoder}`
//!   implementation) for packet framing, enforcing [`MAX_PACKET_SIZE`];
//! * [`Handshake`], the only structured packet implemented here;
//! * [`parse_login_start_name`], a minimal partial parser for the Login Start
//!   packet's leading fields.
//!
//! The format authority is <https://minecraft.wiki/w/Java_Edition_protocol>.

#![forbid(unsafe_code)]

mod error;
mod handshake;
mod login;
mod packet;
mod string;
mod uuid;
mod varint;

pub use error::ProtocolError;
pub use handshake::{Handshake, Intent};
pub use login::parse_login_start_name;
pub use packet::{MAX_PACKET_SIZE, Packet, PacketCodec};
pub use string::{DEFAULT_MAX_STRING_LENGTH, read_string, write_string};
pub use uuid::{read_uuid, write_uuid};
pub use varint::{decode_varint, decode_varlong, write_varint, write_varlong};
