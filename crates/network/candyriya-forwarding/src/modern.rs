//! The modern (HMAC-SHA256 sealed) forwarding payload.
//!
//! This is the pull-model format a backend server asks for over the
//! `velocity:player_info` login plugin channel. The proxy answers with a
//! [`create_forwarding_data`] payload as the plugin-response data; the
//! backend checks it with the mirror of what [`verify_forwarding_data`]
//! does here.
//!
//! Layout:
//!
//! ```text
//! signature (32 bytes) || VarInt(FORWARDING_VERSION) ||
//!     String(client_address) || UUID (msb i64 BE || lsb i64 BE) ||
//!     String(name) || VarInt(property_count) || properties...
//! property := String(name) || String(value) || bool(has_signature) ||
//!     [String(signature)]
//! ```
//!
//! `signature` is HMAC-SHA256 over every byte after itself, keyed with the
//! forwarding secret (the raw secret bytes, as configured). `bool` is the
//! protocol boolean: `0x01` when a signature follows, `0x00` otherwise.
//! `FORWARDING_VERSION` is always 1 for the base format; the keyed
//! `MODERN_FORWARDING_WITH_KEY` variant (2) carries a chat-signing public
//! key and is out of scope here.
//!
//! Verified against the reference implementations: Velocity's
//! `LoginSessionHandler#createForwardingData` and Paper's
//! `VelocityProxy#checkIntegrity` / `createProfile`. Both read 32
//! signature bytes first, authenticate the remainder with a constant-time
//! comparison, and only then parse the fields. This module does the same so
//! a hostile payload never reaches the parser in a half-trusted state.
//!
//! `client_address` is the textual IP of the connecting client *without* a
//! port and with any IPv6 `%scope` suffix stripped (see
//! [`clean_ipv6_scope`]). The payload does not carry or check the port.

use std::borrow::Cow;

use bytes::{BufMut, BytesMut};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

use candyriya_player::{GameProfile, Property};
use candyriya_protocol::{
    DEFAULT_MAX_STRING_LENGTH, decode_varint, read_string, read_uuid, write_string, write_uuid,
    write_varint,
};

use crate::error::ForwardingError;

/// The forwarding protocol version this module creates and accepts.
///
/// Always `1` (`MODERN_FORWARDING_DEFAULT`): no chat public key is
/// forwarded, because the proxy does not hold one.
pub const FORWARDING_VERSION: i32 = 1;

/// Length of the HMAC-SHA256 signature prefix.
pub const SIGNATURE_LENGTH: usize = 32;

type HmacSha256 = Hmac<Sha256>;

fn hmac_sha256(secret: &[u8], payload: &[u8]) -> [u8; SIGNATURE_LENGTH] {
    // HMAC accepts a key of any length, so this cannot fail; the key is
    // used exactly as configured, matching the reference implementations.
    let mut mac = HmacSha256::new_from_slice(secret)
        .unwrap_or_else(|_| unreachable!("HMAC-SHA256 accepts keys of any length"));
    mac.update(payload);
    let out = mac.finalize().into_bytes();
    let mut bytes = [0u8; SIGNATURE_LENGTH];
    bytes.copy_from_slice(&out);
    bytes
}

/// Constant-time byte equality.
///
/// XOR-accumulate over the whole buffers, no early exit, so the comparison
/// does not leak how many leading bytes matched. Lengths differing is not a
/// secret (the signature is always 32 bytes), so a length check up front is
/// fine.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Strip the `%scope` suffix from an IPv6 address string, if any.
///
/// Mirrors what the reference implementations do for `%zone` suffixes
/// (`fe80::1%eth0` becomes `fe80::1`) and is safe for IPv4 strings, which
/// never contain a `%` in a valid address: they are returned unchanged.
/// Strips at the first `%`, exactly like the reference does.
pub fn clean_ipv6_scope(address: &str) -> Cow<'_, str> {
    match address.find('%') {
        Some(index) => Cow::Owned(address[..index].to_owned()),
        None => Cow::Borrowed(address),
    }
}

/// Build the sealed forwarding payload for `profile`.
///
/// `address` is the client's remote IP *without* a port; any IPv6 `%scope`
/// suffix is stripped with [`clean_ipv6_scope`]. The result is the bytes
/// the backend expects as the login plugin response data: the 32-byte
/// HMAC-SHA256 signature followed by the payload it authenticates.
///
/// # Panics
///
/// Panics if a string does not fit the wire format: more than 32767 UTF-16
/// code units for the address or a property string, or more than 16 units
/// for the account name (the format caps the name at 16, matching the
/// reference readers). Mojang names are at most 16 units and property
/// values are far below the cap, so this can only happen for inputs the
/// format itself cannot represent.
pub fn create_forwarding_data(secret: &[u8], address: &str, profile: &GameProfile) -> Vec<u8> {
    let cleaned = clean_ipv6_scope(address);

    let mut payload = BytesMut::new();
    write_varint(FORWARDING_VERSION, &mut payload);
    write_string(&mut payload, &cleaned, DEFAULT_MAX_STRING_LENGTH)
        .expect("address fits the wire string limit");
    let (msb, lsb) = profile.id.as_u64_pair();
    write_uuid(msb as i64, lsb as i64, &mut payload);
    write_string(&mut payload, &profile.name, 16)
        .expect("account names are at most 16 UTF-16 code units");
    let property_count = i32::try_from(profile.properties.len()).unwrap_or(i32::MAX);
    write_varint(property_count, &mut payload);
    for property in &profile.properties {
        write_string(&mut payload, &property.name, DEFAULT_MAX_STRING_LENGTH)
            .expect("property name fits the wire string limit");
        write_string(&mut payload, &property.value, DEFAULT_MAX_STRING_LENGTH)
            .expect("property value fits the wire string limit");
        match &property.signature {
            Some(signature) => {
                payload.put_u8(0x01);
                write_string(&mut payload, signature, DEFAULT_MAX_STRING_LENGTH)
                    .expect("property signature fits the wire string limit");
            }
            None => payload.put_u8(0x00),
        }
    }

    let signature = hmac_sha256(secret, &payload);
    let mut sealed = Vec::with_capacity(SIGNATURE_LENGTH + payload.len());
    sealed.extend_from_slice(&signature);
    sealed.extend_from_slice(&payload);
    sealed
}

/// Verify a sealed forwarding payload as received from a proxy.
///
/// Reads and authenticates the bytes the way the reference backend does:
/// first the 32-byte signature (HMAC-SHA256 over the remainder, compared in
/// constant time), then the version, address, UUID, name and properties.
/// Returns the client's remote IP as raw bytes and the decoded profile on
/// success. No network access and no parsing of the address happens here;
/// deciding whether the string is a usable IP is the caller's job.
///
/// # Errors
///
/// Returns [`ForwardingError::Truncated`] for short input or an
/// unterminated value, [`ForwardingError::BadSignature`] for a signature
/// that does not authenticate the payload,
/// [`ForwardingError::UnsupportedVersion`] for a version other than 1,
/// [`ForwardingError::TrailingBytes`] for well-formed data followed by
/// extra bytes, or [`ForwardingError::Protocol`] for malformed wire values.
pub fn verify_forwarding_data(
    secret: &[u8],
    bytes: &[u8],
) -> Result<(Vec<u8>, GameProfile), ForwardingError> {
    if bytes.len() < SIGNATURE_LENGTH {
        return Err(ForwardingError::Truncated);
    }
    let (signature, payload) = bytes.split_at(SIGNATURE_LENGTH);
    let expected = hmac_sha256(secret, payload);
    if !constant_time_eq(signature, &expected) {
        return Err(ForwardingError::BadSignature);
    }

    let mut buf = payload;
    let version = decode_varint(&mut buf)?;
    if version != FORWARDING_VERSION {
        return Err(ForwardingError::UnsupportedVersion(version));
    }

    let ip = read_string(&mut buf, DEFAULT_MAX_STRING_LENGTH)?;
    let (msb, lsb) = read_uuid(&mut buf)?;
    let name = read_string(&mut buf, 16)?;

    let property_count = decode_varint(&mut buf)?;
    if property_count < 0 {
        return Err(ForwardingError::InvalidPropertyCount(property_count));
    }
    // no pre-allocation from the declared count: a hostile payload could
    // declare billions of properties and force an absurd allocation.
    // Each property costs at least 3 bytes (two empty strings + boolean),
    // so a count beyond the remaining bytes is already bogus.
    if property_count as usize > buf.len() / 3 + 1 {
        return Err(ForwardingError::InvalidPropertyCount(property_count));
    }
    let mut properties = Vec::new();
    for _ in 0..property_count {
        let name = read_string(&mut buf, DEFAULT_MAX_STRING_LENGTH)?;
        let value = read_string(&mut buf, DEFAULT_MAX_STRING_LENGTH)?;
        let has_signature = match buf.first() {
            Some(&0x00) => false,
            Some(&0x01) => true,
            // any other byte is not a valid protocol boolean
            Some(&other) => return Err(ForwardingError::InvalidBoolean(other)),
            None => return Err(ForwardingError::Truncated),
        };
        buf = &buf[1..];
        let signature = if has_signature {
            Some(read_string(&mut buf, DEFAULT_MAX_STRING_LENGTH)?)
        } else {
            None
        };
        properties.push(Property {
            name,
            value,
            signature,
        });
    }

    if !buf.is_empty() {
        return Err(ForwardingError::TrailingBytes(buf.len()));
    }

    Ok((
        ip.into_bytes(),
        GameProfile {
            id: Uuid::from_u64_pair(msb as u64, lsb as u64),
            name,
            properties,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_bytes_inside_a_validly_sealed_payload_are_rejected() {
        use bytes::BytesMut;
        use candyriya_protocol::{write_string, write_uuid, write_varint};

        // seal a payload with one extra byte after the properties section;
        // the signature is valid, so only the trailing-bytes check can catch it
        let mut payload = BytesMut::new();
        write_varint(FORWARDING_VERSION, &mut payload);
        write_string(&mut payload, "203.0.113.7", DEFAULT_MAX_STRING_LENGTH).unwrap();
        write_uuid(0, 0, &mut payload);
        write_string(&mut payload, "Notch", DEFAULT_MAX_STRING_LENGTH).unwrap();
        write_varint(0, &mut payload); // zero properties
        payload.put_u8(0x00); // not part of the format

        let signature = hmac_sha256(b"secret", &payload);
        let mut sealed = signature.to_vec();
        sealed.extend_from_slice(&payload);

        let result = verify_forwarding_data(b"secret", &sealed);
        assert!(matches!(result, Err(ForwardingError::TrailingBytes(1))));
    }

    #[test]
    fn invalid_boolean_inside_a_validly_sealed_payload_is_rejected() {
        use bytes::BytesMut;
        use candyriya_protocol::{write_string, write_uuid, write_varint};

        let mut payload = BytesMut::new();
        write_varint(FORWARDING_VERSION, &mut payload);
        write_string(&mut payload, "203.0.113.7", DEFAULT_MAX_STRING_LENGTH).unwrap();
        write_uuid(0, 0, &mut payload);
        write_string(&mut payload, "Notch", DEFAULT_MAX_STRING_LENGTH).unwrap();
        write_varint(1, &mut payload); // exactly one property
        write_string(&mut payload, "textures", DEFAULT_MAX_STRING_LENGTH).unwrap();
        write_string(&mut payload, "dGVzdA==", DEFAULT_MAX_STRING_LENGTH).unwrap();
        payload.put_u8(0x02); // not a valid protocol boolean

        let signature = hmac_sha256(b"secret", &payload);
        let mut sealed = signature.to_vec();
        sealed.extend_from_slice(&payload);

        let result = verify_forwarding_data(b"secret", &sealed);
        assert!(matches!(result, Err(ForwardingError::InvalidBoolean(0x02))));
    }

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"ab", b"abc"));
        // differing only in the last byte still differs
        assert!(!constant_time_eq(
            b"signature-32-bytes-long-padded.....",
            b"signature-32-bytes-long-padded....!"
        ));
    }

    #[test]
    fn clean_ipv6_scope_strips() {
        assert_eq!(clean_ipv6_scope("fe80::1%eth0"), "fe80::1");
        assert_eq!(clean_ipv6_scope("fe80::1%25eth0"), "fe80::1");
        assert_eq!(clean_ipv6_scope("fe80::1%"), "fe80::1");
        assert_eq!(
            clean_ipv6_scope("::ffff:192.0.2.1%scope"),
            "::ffff:192.0.2.1"
        );
    }

    #[test]
    fn clean_ipv6_scope_leaves_others_alone() {
        assert_eq!(clean_ipv6_scope("127.0.0.1"), "127.0.0.1");
        assert_eq!(clean_ipv6_scope("2001:db8::1"), "2001:db8::1");
        assert_eq!(clean_ipv6_scope(""), "");
        assert!(matches!(clean_ipv6_scope("2001:db8::1"), Cow::Borrowed(_)));
        assert!(matches!(clean_ipv6_scope("fe80::1%eth0"), Cow::Owned(_)));
    }
}
