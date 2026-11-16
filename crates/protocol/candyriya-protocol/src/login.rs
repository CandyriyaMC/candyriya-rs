//! Minimal parsing of the Login Start packet's leading fields.
//!
//! A serverbound Login Start packet begins with:
//!
//! 1. a packet id `VarInt` (`0x00` on all versions this crate targets),
//! 2. the player's username as `String(16)`.
//!
//! Everything after that — anything from a single trailing UUID to a full
//! 1.19+ chat-signature block — differs between protocol versions, so this
//! crate parses only the first two fields and leaves the tail untouched.
//! That is all a version-agnostic forwarding layer needs to know about.

use crate::error::ProtocolError;
use crate::string::read_string;
use crate::varint::decode_varint;

/// Maximum length of the username field, in UTF-16 code units.
///
/// The wiki types the username as `String(16)`.
const LOGIN_START_NAME_MAX_LENGTH: usize = 16;

/// Extracts the username from a Login Start packet's payload.
///
/// `payload` must be the packet data **after** the framing length prefix,
/// exactly as delivered by [`crate::Packet`]. Only the packet id `VarInt`
/// and the leading username string are consumed; the rest of the payload is
/// not even looked at.
///
/// Returns [`ProtocolError::Truncated`] if the payload ends mid-field,
/// [`ProtocolError::VarIntTooBig`] if the packet id `VarInt` overflows,
/// [`ProtocolError::StringTooLong`] if the declared string is over the
/// `String(16)` cap, or [`ProtocolError::InvalidUtf8`] if it is not UTF-8.
pub fn parse_login_start_name(payload: &[u8]) -> Result<String, ProtocolError> {
    let mut src = payload;
    // packet id is read only to advance the cursor; its meaning depends on the connection state
    let _packet_id = decode_varint(&mut src)?;
    read_string(&mut src, LOGIN_START_NAME_MAX_LENGTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(id: i32, name: &str, trailing: &[u8]) -> Vec<u8> {
        let mut buffer = bytes::BytesMut::new();
        crate::varint::write_varint(id, &mut buffer);
        crate::string::write_string(&mut buffer, name, 16).unwrap();
        buffer.extend_from_slice(trailing);
        buffer.to_vec()
    }

    #[test]
    fn parses_name_ignoring_trailing_bytes() {
        // login-style payload: id 0x00, string, then a trailing UUID and signature-like blob
        let mut packet = bytes::BytesMut::new();
        crate::varint::write_varint(0x00, &mut packet);
        crate::string::write_string(&mut packet, "Steve", 16).unwrap();
        packet.extend_from_slice(&[0x00; 16]); // player uuid: ignored by design
        packet.extend_from_slice(&[0xAB; 228]); // trailing signature block: also ignored
        assert_eq!(parse_login_start_name(&packet).unwrap(), "Steve");
    }

    #[test]
    fn parses_1_21_style_captured_packet() {
        // captured-style 1.21 login start: id 0x00, name, then trailing signature field
        // (the exact signature layout is intentionally not reconstructed; the parser must not care)
        let packet = payload(
            0x00,
            "Notch",
            &[
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // property-ish bytes
                0xDE, 0xAD, 0xBE, 0xEF, // junk standing in for the signature block
            ],
        );
        assert_eq!(parse_login_start_name(&packet).unwrap(), "Notch");
    }

    #[test]
    fn packet_id_varint_may_be_multi_byte() {
        let packet = payload(25565, "Herobrine", &[]);
        assert_eq!(parse_login_start_name(&packet).unwrap(), "Herobrine");
    }

    #[test]
    fn unicode_name_roundtrips() {
        let packet = payload(0x00, "Пестр", &[0xAA; 32]);
        assert_eq!(parse_login_start_name(&packet).unwrap(), "Пестр");
    }

    #[test]
    fn empty_payload_is_truncated() {
        assert!(matches!(
            parse_login_start_name(&[]),
            Err(ProtocolError::Truncated)
        ));
    }

    #[test]
    fn missing_string_field_is_truncated() {
        assert!(matches!(
            parse_login_start_name(&[0x00]),
            Err(ProtocolError::Truncated)
        ));
    }

    #[test]
    fn truncated_string_payload_is_truncated() {
        // id 0x00, string length prefix 0x05, but only 2 bytes present
        let packet = [0x00, 0x05, b'a', b'b'];
        assert!(matches!(
            parse_login_start_name(&packet),
            Err(ProtocolError::Truncated)
        ));
    }

    #[test]
    fn invalid_utf8_name_is_rejected() {
        let packet = [0x00, 0x02, 0xFF, 0xFE];
        assert!(matches!(
            parse_login_start_name(&packet),
            Err(ProtocolError::InvalidUtf8)
        ));
    }

    #[test]
    fn overlong_name_is_rejected() {
        // 17 units for a String(16) field; crafted by hand because write_string rightly refuses it
        let mut packet = bytes::BytesMut::new();
        crate::varint::write_varint(0x00, &mut packet);
        packet.extend_from_slice(&[0x11]); // string length prefix: 17 bytes
        packet.extend_from_slice(b"seventeen_chars_"); // exactly 16
        packet.extend_from_slice(b"x");
        assert_eq!(packet.len(), 2 + 17);
        assert!(matches!(
            parse_login_start_name(&packet),
            Err(ProtocolError::StringTooLong)
        ));
    }

    #[test]
    fn overflowing_packet_id_is_rejected() {
        let packet = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
        assert!(matches!(
            parse_login_start_name(&packet),
            Err(ProtocolError::VarIntTooBig)
        ));
    }
}
