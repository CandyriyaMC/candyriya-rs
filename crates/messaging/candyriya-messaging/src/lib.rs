//! Wire-format core for proxy plugin messaging.
//!
//! This crate is the shared byte-level foundation the
//! `candyriya-messaging-bungeecord` and `candyriya-messaging-velocity`
//! crates build on. It provides exactly four things and nothing more:
//!
//! - the Java `DataOutputStream` modified-UTF-8 codec
//!   ([`java::write_java_string`] / [`java::read_java_string`]) plus the
//!   companion big-endian primitives the backend channel protocols use
//!   ([`java::write_java_int`], [`java::read_java_i64`], and friends),
//! - [`ChannelIdentifier`], a validated newtype for channel names in both
//!   syntaxes (modern `namespace:key` and legacy single-part names),
//! - [`PluginMessage`], the channel + opaque payload that crosses the wire,
//!   and [`PluginMessageBuilder`] / [`MessageBuffer`] to assemble payloads,
//! - the error types for the above.
//!
//! # Scope
//!
//! This crate is deliberately **protocol-agnostic**. It knows nothing about
//! Connect/IP/UUID/PlayerCount or any other subchannel semantics — those
//! are layered on top in the backend-specific crates. There is no network,
//! no async runtime, no unsafe and no dependency beyond `bytes` and
//! `thiserror`.
//!
//! # Example
//!
//! ```
//! use candyriya_messaging::{ChannelIdentifier, PluginMessageBuilder, java::read_java_string};
//!
//! let channel = ChannelIdentifier::new("example:main")?;
//! let mut builder = PluginMessageBuilder::new(channel);
//! builder.write_java_string("hello")?;
//! builder.write_java_int(7);
//! let message = builder.build();
//!
//! let mut cursor = message.data.as_ref();
//! assert_eq!(read_java_string(&mut cursor, 64)?, "hello");
//! assert_eq!(candyriya_messaging::java::read_java_int(&mut cursor)?, 7);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Safety
//!
//! `#![forbid(unsafe_code)]`; all fallible operations report errors through
//! return values, and the decoder is total on arbitrary bytes.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod buffer;
pub mod channel;
pub mod java;
pub mod message;

pub use buffer::MessageBuffer;
pub use channel::{ChannelIdentifier, ChannelIdentifierError, ChannelSegment, MAX_CHANNEL_LEN};
pub use java::{
    JavaCodecError, MAX_UTF_BYTES, read_java_bool, read_java_i64, read_java_int, read_java_string,
    read_java_u8, read_java_u16, write_java_bool, write_java_i64, write_java_int,
    write_java_string, write_java_u8, write_java_u16,
};
pub use message::{PluginMessage, PluginMessageBuilder};
