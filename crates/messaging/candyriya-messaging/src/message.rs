//! The unit of data that crosses the plugin-message wire.
//!
//! A [`PluginMessage`] is just a channel plus an opaque byte payload — this
//! crate deliberately assigns no meaning to the bytes. Backend-specific
//! subchannel protocols live in the `-bungeecord`/`-velocity` crates built
//! on top of this one.

use bytes::Bytes;

use crate::buffer::MessageBuffer;
use crate::channel::ChannelIdentifier;
use crate::java::JavaCodecError;

/// A plugin message: a channel and an opaque payload.
///
/// `data` is whatever the backend protocol put on the wire; this crate
/// never inspects it. Use [`java::read_java_string`](crate::java::read_java_string)
/// and friends to decode subchannel payloads on top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMessage {
    /// The channel the message belongs to.
    pub channel: ChannelIdentifier,
    /// The payload bytes, verbatim as they appeared on the wire.
    pub data: Bytes,
}

impl PluginMessage {
    /// Builds a message from a channel and any byte container.
    pub fn new(channel: ChannelIdentifier, data: impl Into<Bytes>) -> Self {
        Self {
            channel,
            data: data.into(),
        }
    }
}

/// Convenience builder that assembles a [`PluginMessage`] payload with
/// Java-stream write helpers.
///
/// Exists so backend-protocol code can write fields in wire order without
/// threading a buffer through every call:
///
/// ```
/// use candyriya_messaging::{ChannelIdentifier, PluginMessageBuilder};
///
/// let mut builder = PluginMessageBuilder::new(ChannelIdentifier::new("example:main")?);
/// builder.write_java_string("hello")?;
/// builder.write_java_int(42);
/// let message = builder.build();
/// assert_eq!(message.channel.as_str(), "example:main");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct PluginMessageBuilder {
    channel: ChannelIdentifier,
    buf: MessageBuffer,
}

impl PluginMessageBuilder {
    /// A builder for `channel` with no reserved payload capacity.
    pub fn new(channel: ChannelIdentifier) -> Self {
        Self {
            channel,
            buf: MessageBuffer::new(),
        }
    }

    /// A builder for `channel` with space for at least `capacity` payload
    /// bytes pre-reserved.
    pub fn with_capacity(channel: ChannelIdentifier, capacity: usize) -> Self {
        Self {
            channel,
            buf: MessageBuffer::with_capacity(capacity),
        }
    }

    /// The channel the built message will carry.
    pub fn channel(&self) -> &ChannelIdentifier {
        &self.channel
    }

    /// Number of payload bytes written so far.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written yet (since the last [`clear`](Self::clear)).
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The payload bytes written so far.
    pub fn as_slice(&self) -> &[u8] {
        self.buf.as_slice()
    }

    /// Drops all payload bytes, keeping the channel and the reserved capacity.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Appends raw bytes without any framing.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.buf.write_bytes(bytes);
    }

    /// Writes a string as Java `DataOutputStream.writeUTF` would;
    /// see [`java::write_java_string`](crate::java::write_java_string).
    pub fn write_java_string(&mut self, s: &str) -> Result<(), JavaCodecError> {
        self.buf.write_java_string(s)
    }

    /// Writes a single byte; see [`java::write_java_u8`](crate::java::write_java_u8).
    pub fn write_java_u8(&mut self, value: u8) {
        self.buf.write_java_u8(value);
    }

    /// Writes a two-byte big-endian unsigned integer;
    /// see [`java::write_java_u16`](crate::java::write_java_u16).
    pub fn write_java_u16(&mut self, value: u16) {
        self.buf.write_java_u16(value);
    }

    /// Writes a four-byte big-endian signed integer;
    /// see [`java::write_java_int`](crate::java::write_java_int).
    pub fn write_java_int(&mut self, value: i32) {
        self.buf.write_java_int(value);
    }

    /// Writes an eight-byte big-endian signed integer;
    /// see [`java::write_java_i64`](crate::java::write_java_i64).
    pub fn write_java_i64(&mut self, value: i64) {
        self.buf.write_java_i64(value);
    }

    /// Writes a Java boolean; see [`java::write_java_bool`](crate::java::write_java_bool).
    pub fn write_java_bool(&mut self, value: bool) {
        self.buf.write_java_bool(value);
    }

    /// Consumes the builder and returns the assembled message.
    pub fn build(self) -> PluginMessage {
        PluginMessage {
            channel: self.channel,
            data: self.buf.into_bytes(),
        }
    }
}
