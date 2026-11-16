//! The Java stream codec: modified UTF-8 (CESU-8) and the big-endian
//! primitives `DataOutputStream`/`DataInputStream` use.
//!
//! Java does **not** write strings as plain UTF-8. `writeUTF` writes a
//! two-byte unsigned big-endian count of the payload bytes, followed by the
//! string encoded in *modified UTF-8* (often called CESU-8):
//!
//! | code point          | encoding                                        | bytes |
//! |---------------------|-------------------------------------------------|-------|
//! | U+0000              | `0xC0 0x80` (the only allowed overlong form)    | 2     |
//! | U+0001..=U+007F     | `0x01..=0x7F`                                   | 1     |
//! | U+0080..=U+07FF     | `110xxxxx 10xxxxxx`                             | 2     |
//! | U+0800..=U+FFFF     | `1110xxxx 10xxxxxx 10xxxxxx` (surrogates incl.) | 3     |
//! | U+10000..=U+10FFFF  | UTF-16 surrogate pair, each half as 3 bytes     | 6     |
//!
//! Rust `char`s never *are* unpaired surrogates, so UTF-8 input can always be
//! mapped onto this table; decoding is the strict direction. The decoder
//! rejects anything that is not valid CESU-8, so bytes produced by a hostile
//! or broken backend can never turn into an allocation above `max_bytes` or
//! a panic.
//!
//! # Deliberate strictness vs. the JVM
//!
//! - The JVM's `readUTF` does not pair surrogates; this decoder combines
//!   each high/low surrogate pair into the astral `char` it denotes, because
//!   Rust `String` cannot hold lone surrogates. An unpaired surrogate is
//!   rejected with [`JavaCodecError::LoneSurrogate`].
//! - The JVM's `readBoolean` treats any non-zero byte as `true`;
//!   [`read_java_bool`] requires exactly `0x00`/`0x01`, which is all
//!   `writeBoolean` ever emits. Strictness here turns silent corruption
//!   into a loud, located error.
//! - The JVM neither rejects overlong forms nor `0xC0`/`0xC1` leads;
//!   this decoder does, per the modified-UTF-8 table above.

use thiserror::Error;

/// Maximum byte count that fits the two-byte length prefix of `writeUTF`.
pub const MAX_UTF_BYTES: usize = u16::MAX as usize;

/// Errors produced by the Java stream codec.
///
/// Every variant names the byte offset that caused the failure (relative to
/// the start of the string payload when decoding a string) so callers can
/// build actionable error messages without re-walking the input.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JavaCodecError {
    /// The encoded payload does not fit the two-byte length prefix.
    #[error(
        "encoded string is {byte_len} bytes, but the Java modified UTF-8 length prefix is an unsigned 16-bit count of bytes (max 65535); shorten the string"
    )]
    EncodedStringTooLong {
        /// The encoded payload length that does not fit.
        byte_len: usize,
    },
    /// The declared payload length exceeds the caller-declared maximum.
    #[error(
        "declared string length of {byte_len} bytes exceeds the caller-declared maximum of {max_bytes} bytes; refusing to read"
    )]
    ExceedsMaxBytes {
        /// The payload length declared by the input's two-byte prefix.
        byte_len: usize,
        /// The caller-declared ceiling the length was checked against.
        max_bytes: usize,
    },
    /// Input ended before the requested number of bytes was available.
    #[error("unexpected end of data: needed {needed} byte(s), but only {remaining} remain")]
    Truncated {
        /// Bytes the read needed.
        needed: usize,
        /// Bytes actually available.
        remaining: usize,
    },
    /// A multi-byte sequence runs past the end of the declared payload.
    #[error(
        "CESU-8 sequence at string offset {offset} needs {needed} byte(s), but only {remaining} remain in the string payload"
    )]
    TruncatedSequence {
        /// Offset of the sequence start within the string payload.
        offset: usize,
        /// Bytes the sequence needed.
        needed: usize,
        /// Bytes remaining in the declared payload.
        remaining: usize,
    },
    /// A byte that cannot begin a modified-UTF-8 sequence.
    #[error(
        "invalid CESU-8 start byte 0x{byte:02X} at string offset {offset}; valid start bytes are 0x01..=0x7F, 0xC2..=0xDF and 0xE0..=0xEF (U+0000 is written as 0xC0 0x80)"
    )]
    InvalidStartByte {
        /// Offset within the string payload.
        offset: usize,
        /// The offending byte.
        byte: u8,
    },
    /// A continuation byte with the wrong value (or a non-continuation where one was expected).
    #[error(
        "invalid CESU-8 continuation byte 0x{byte:02X} at string offset {offset}; expected a byte in 0x80..=0xBF"
    )]
    InvalidContinuation {
        /// Offset of the offending byte within the string payload.
        offset: usize,
        /// The offending byte.
        byte: u8,
    },
    /// A multi-byte form used where a shorter form exists.
    #[error(
        "overlong CESU-8 encoding at string offset {offset}; every character must use its shortest form, with the single exception of U+0000, which is written 0xC0 0x80"
    )]
    OverlongEncoding {
        /// Offset of the sequence start within the string payload.
        offset: usize,
    },
    /// A UTF-16 surrogate half that has no matching partner.
    #[error(
        "unpaired UTF-16 surrogate 0x{surrogate:04X} at string offset {offset}; astral-plane characters must appear as a high surrogate (0xD800..=0xDBFF) followed by a low surrogate (0xDC00..=0xDFFF), and a Rust String cannot hold a lone surrogate"
    )]
    LoneSurrogate {
        /// Offset of the surrogate's sequence start within the string payload.
        offset: usize,
        /// The unpaired surrogate value (0xD800..=0xDFFF).
        surrogate: u16,
    },
    /// A boolean byte other than `0x00`/`0x01`.
    #[error("invalid boolean byte 0x{byte:02X}; expected 0x00 (false) or 0x01 (true)")]
    InvalidBoolean {
        /// The offending byte.
        byte: u8,
    },
}

fn encoded_len(s: &str) -> usize {
    let mut len = 0usize;
    for ch in s.chars() {
        len += match ch as u32 {
            0x0000 => 2,
            0x0001..=0x007F => 1,
            0x0080..=0x07FF => 2,
            0x0800..=0xFFFF => 3,
            _ => 6, // astral: surrogate pair, 3 bytes per half
        };
    }
    len
}

/// Writes `s` into `buf` as `DataOutputStream.writeUTF` does: a two-byte
/// big-endian count of the payload bytes, then the modified-UTF-8 (CESU-8)
/// payload itself.
///
/// Fails with [`JavaCodecError::EncodedStringTooLong`] before touching `buf`
/// when the payload exceeds 65535 bytes, matching the JVM, which throws
/// `UTFDataFormatException` without writing anything.
pub fn write_java_string(buf: &mut Vec<u8>, s: &str) -> Result<(), JavaCodecError> {
    let byte_len = encoded_len(s);
    if byte_len > MAX_UTF_BYTES {
        return Err(JavaCodecError::EncodedStringTooLong { byte_len });
    }
    write_java_u16(buf, byte_len as u16);
    for ch in s.chars() {
        let cp = ch as u32;
        match cp {
            0x0000 => buf.extend_from_slice(&[0xC0, 0x80]),
            0x0001..=0x007F => buf.push(cp as u8),
            0x0080..=0x07FF => {
                buf.push((0xC0 | (cp >> 6)) as u8);
                buf.push((0x80 | (cp & 0x3F)) as u8);
            }
            0x0800..=0xFFFF => {
                buf.push((0xE0 | (cp >> 12)) as u8);
                buf.push((0x80 | ((cp >> 6) & 0x3F)) as u8);
                buf.push((0x80 | (cp & 0x3F)) as u8);
            }
            _ => {
                let t = cp - 0x10000;
                let high = 0xD800 + (t >> 10);
                let low = 0xDC00 + (t & 0x3FF);
                for half in [high, low] {
                    buf.push((0xE0 | (half >> 12)) as u8);
                    buf.push((0x80 | ((half >> 6) & 0x3F)) as u8);
                    buf.push((0x80 | (half & 0x3F)) as u8);
                }
            }
        }
    }
    Ok(())
}

fn take<'a>(buf: &mut &'a [u8], n: usize) -> Result<&'a [u8], JavaCodecError> {
    if buf.len() < n {
        return Err(JavaCodecError::Truncated {
            needed: n,
            remaining: buf.len(),
        });
    }
    let (head, rest) = buf.split_at(n);
    *buf = rest;
    Ok(head)
}

/// Splits off exactly one byte, wherever the caller decided it must exist.
fn byte_at(payload: &[u8], offset: usize) -> Result<u8, JavaCodecError> {
    match payload.get(offset) {
        Some(&b) => Ok(b),
        None => Err(JavaCodecError::TruncatedSequence {
            offset,
            needed: 1,
            remaining: 0,
        }),
    }
}

fn continuation(payload: &[u8], offset: usize) -> Result<u8, JavaCodecError> {
    match payload.get(offset) {
        Some(&b) if (b & 0xC0) == 0x80 => Ok(b),
        Some(&b) => Err(JavaCodecError::InvalidContinuation { offset, byte: b }),
        None => Err(JavaCodecError::TruncatedSequence {
            offset,
            needed: 1,
            remaining: 0,
        }),
    }
}

fn seq_bound(payload: &[u8], offset: usize, needed: usize) -> Result<(), JavaCodecError> {
    let remaining = payload.len().saturating_sub(offset);
    if remaining < needed {
        return Err(JavaCodecError::TruncatedSequence {
            offset,
            needed,
            remaining,
        });
    }
    Ok(())
}

/// Reads a Java `DataInputStream.readUTF` string: a two-byte big-endian
/// count of payload bytes, then the modified-UTF-8 payload.
///
/// `max_bytes` is the caller's hard ceiling on the *payload* length, checked
/// before any payload byte is consumed, so a hostile two-byte length cannot
/// force an oversized allocation. The full u16 range (65535) is always
/// accepted as the prefix value itself; pass a smaller `max_bytes` to cap
/// further, or `65535` to accept anything the format allows.
///
/// Astral-plane characters encoded as surrogate pairs are combined back
/// into one `char`; an unpaired surrogate is rejected, because a Rust
/// `String` cannot hold one.
///
/// The two length-prefix bytes are consumed only when at least two bytes
/// remain; on any error the cursor position is unspecified, and callers
/// should stop parsing the buffer rather than try to resynchronize.
pub fn read_java_string(buf: &mut &[u8], max_bytes: usize) -> Result<String, JavaCodecError> {
    let byte_len = read_java_u16(buf)? as usize;
    if byte_len > max_bytes {
        return Err(JavaCodecError::ExceedsMaxBytes {
            byte_len,
            max_bytes,
        });
    }
    let payload = take(buf, byte_len)?;
    decode_payload(payload)
}

fn decode_payload(payload: &[u8]) -> Result<String, JavaCodecError> {
    let mut out = String::with_capacity(payload.len());
    let mut offset = 0usize;
    while offset < payload.len() {
        let b0 = payload[offset];
        match b0 {
            0x01..=0x7F => {
                out.push(b0 as char);
                offset += 1;
            }
            0xC0 => {
                seq_bound(payload, offset, 2)?;
                let b1 = byte_at(payload, offset + 1)?;
                if b1 == 0x80 {
                    out.push('\0'); // the only legal 0xC0 pair
                } else if (b1 & 0xC0) == 0x80 {
                    return Err(JavaCodecError::OverlongEncoding { offset });
                } else {
                    return Err(JavaCodecError::InvalidContinuation {
                        offset: offset + 1,
                        byte: b1,
                    });
                }
                offset += 2;
            }
            0xC1 => {
                // 0x40 | (b1 & 0x3F) is always < 0x80, so every 0xC1 lead is
                // an overlong two-byte form of an ASCII code point.
                seq_bound(payload, offset, 2)?;
                continuation(payload, offset + 1)?;
                return Err(JavaCodecError::OverlongEncoding { offset });
            }
            0xC2..=0xDF => {
                seq_bound(payload, offset, 2)?;
                let b1 = continuation(payload, offset + 1)?;
                let cp = (((b0 & 0x1F) as u32) << 6) | (b1 & 0x3F) as u32;
                push_scalar(&mut out, cp, offset)?;
                offset += 2;
            }
            0xE0..=0xEF => {
                seq_bound(payload, offset, 3)?;
                let b1 = continuation(payload, offset + 1)?;
                let b2 = continuation(payload, offset + 2)?;
                let cp =
                    (((b0 & 0x0F) as u32) << 12) | (((b1 & 0x3F) as u32) << 6) | (b2 & 0x3F) as u32;
                if cp < 0x800 {
                    return Err(JavaCodecError::OverlongEncoding { offset });
                }
                match cp {
                    0xD800..=0xDBFF => {
                        // high surrogate: must be followed by a 3-byte low surrogate
                        seq_bound(payload, offset, 6)?;
                        let b3 = byte_at(payload, offset + 3)?;
                        if b3 != 0xED {
                            return Err(JavaCodecError::LoneSurrogate {
                                offset,
                                surrogate: cp as u16,
                            });
                        }
                        let b4 = continuation(payload, offset + 4)?;
                        let b5 = continuation(payload, offset + 5)?;
                        let low = (((b4 & 0x3F) as u32) << 6) | (b5 & 0x3F) as u32; // b3 is 0xED => 0xD000 base
                        let low = 0xD000 | low;
                        if !(0xDC00..=0xDFFF).contains(&low) {
                            return Err(JavaCodecError::LoneSurrogate {
                                offset,
                                surrogate: cp as u16,
                            });
                        }
                        let combined = 0x10000 + (((cp - 0xD800) << 10) | (low - 0xDC00));
                        push_scalar(&mut out, combined, offset)?;
                        offset += 6;
                    }
                    0xDC00..=0xDFFF => {
                        return Err(JavaCodecError::LoneSurrogate {
                            offset,
                            surrogate: cp as u16,
                        });
                    }
                    _ => {
                        push_scalar(&mut out, cp, offset)?;
                        offset += 3;
                    }
                }
            }
            0x00 | 0x80..=0xBF | 0xF0..=0xFF => {
                return Err(JavaCodecError::InvalidStartByte { offset, byte: b0 });
            }
        }
    }
    Ok(out)
}

/// `char::from_u32` with a decoder-shaped error; null and the astral range
/// are the only values the callers can hand us and both are valid scalars.
fn push_scalar(out: &mut String, cp: u32, offset: usize) -> Result<(), JavaCodecError> {
    match char::from_u32(cp) {
        Some(ch) => {
            out.push(ch);
            Ok(())
        }
        None => Err(JavaCodecError::LoneSurrogate {
            offset,
            surrogate: cp as u16,
        }),
    }
}

/// Writes a single byte.
pub fn write_java_u8(buf: &mut Vec<u8>, value: u8) {
    buf.push(value);
}

/// Writes a two-byte big-endian unsigned integer.
pub fn write_java_u16(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_be_bytes());
}

/// Writes a four-byte big-endian signed integer, as `DataOutputStream.writeInt` does.
pub fn write_java_int(buf: &mut Vec<u8>, value: i32) {
    buf.extend_from_slice(&value.to_be_bytes());
}

/// Writes an eight-byte big-endian signed integer, as `DataOutputStream.writeLong` does.
pub fn write_java_i64(buf: &mut Vec<u8>, value: i64) {
    buf.extend_from_slice(&value.to_be_bytes());
}

/// Writes a Java boolean (`0x01` for `true`, `0x00` for `false`), as `DataOutputStream.writeBoolean` does.
pub fn write_java_bool(buf: &mut Vec<u8>, value: bool) {
    buf.push(u8::from(value));
}

/// Reads a single byte.
pub fn read_java_u8(buf: &mut &[u8]) -> Result<u8, JavaCodecError> {
    Ok(take(buf, 1)?[0])
}

/// Reads a two-byte big-endian unsigned integer.
pub fn read_java_u16(buf: &mut &[u8]) -> Result<u16, JavaCodecError> {
    let bytes = take(buf, 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// Reads a four-byte big-endian signed integer, as `DataInputStream.readInt` does.
pub fn read_java_int(buf: &mut &[u8]) -> Result<i32, JavaCodecError> {
    let bytes = take(buf, 4)?;
    Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Reads an eight-byte big-endian signed integer, as `DataInputStream.readLong` does.
pub fn read_java_i64(buf: &mut &[u8]) -> Result<i64, JavaCodecError> {
    let bytes = take(buf, 8)?;
    let mut arr = [0u8; 8];
    arr.copy_from_slice(bytes);
    Ok(i64::from_be_bytes(arr))
}

/// Reads a Java boolean. Strict: only `0x00` and `0x01` are accepted
/// (see the module docs for why this is stricter than the JVM).
pub fn read_java_bool(buf: &mut &[u8]) -> Result<bool, JavaCodecError> {
    match read_java_u8(buf)? {
        0x00 => Ok(false),
        0x01 => Ok(true),
        byte => Err(JavaCodecError::InvalidBoolean { byte }),
    }
}
