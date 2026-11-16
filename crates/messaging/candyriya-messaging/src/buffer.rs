//! A reusable heap buffer for assembling message payloads.
//!
//! Production keeps one `MessageBuffer` per writer instead of allocating a
//! fresh `Vec<u8>` per message: [`MessageBuffer::clear`] drops the contents
//! but keeps the capacity, so a hot writer stops growing after the first
//! few messages.

use bytes::Bytes;

use crate::java::{self, JavaCodecError};

/// A reusable, growable payload buffer.
///
/// Cheap to clear and refill. Convert to [`Bytes`] with
/// [`MessageBuffer::into_bytes`] when the payload is ready to send.
#[derive(Debug, Default)]
pub struct MessageBuffer {
    buf: Vec<u8>,
}

impl MessageBuffer {
    /// An empty buffer with no reserved capacity.
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// An empty buffer with space for at least `capacity` bytes pre-reserved.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Drops all payload bytes, keeping the reserved capacity.
    pub fn clear(&mut self) {
        self.buf.clear();
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
        &self.buf
    }

    /// Consumes the buffer and returns the payload as [`Bytes`].
    pub fn into_bytes(self) -> Bytes {
        Bytes::from(self.buf)
    }

    /// Appends raw bytes without any framing.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Writes a string as Java `DataOutputStream.writeUTF` would;
    /// see [`java::write_java_string`].
    pub fn write_java_string(&mut self, s: &str) -> Result<(), JavaCodecError> {
        java::write_java_string(&mut self.buf, s)
    }

    /// Writes a single byte; see [`java::write_java_u8`].
    pub fn write_java_u8(&mut self, value: u8) {
        java::write_java_u8(&mut self.buf, value);
    }

    /// Writes a two-byte big-endian unsigned integer; see [`java::write_java_u16`].
    pub fn write_java_u16(&mut self, value: u16) {
        java::write_java_u16(&mut self.buf, value);
    }

    /// Writes a four-byte big-endian signed integer; see [`java::write_java_int`].
    pub fn write_java_int(&mut self, value: i32) {
        java::write_java_int(&mut self.buf, value);
    }

    /// Writes an eight-byte big-endian signed integer; see [`java::write_java_i64`].
    pub fn write_java_i64(&mut self, value: i64) {
        java::write_java_i64(&mut self.buf, value);
    }

    /// Writes a Java boolean; see [`java::write_java_bool`].
    pub fn write_java_bool(&mut self, value: bool) {
        java::write_java_bool(&mut self.buf, value);
    }
}
