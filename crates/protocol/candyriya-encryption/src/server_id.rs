//! The `serverId` hash used for the session server join/hasJoined calls.
//!
//! The protocol page calls the digest a "non-standard hexdigest": the SHA-1
//! output is interpreted as a single signed two's-complement big-endian
//! integer and printed in *base 16*, lowercase, without leading zeros, with a
//! leading minus sign if the interpreted number is negative. This matches the
//! Java `BigInteger(digest).toString(16)` behavior the protocol inherited.
//!
//! Note that since 1.7 the server id string sent to the client is empty, so
//! the ASCII part of the hash input is empty for all practical purposes.

use sha1::{Digest, Sha1};

/// Compute the `serverId` value for the session server `join` and `hasJoined`
/// calls.
///
/// * `server_id_initial` — the server id string from the Encryption Request
///   packet; pass `""` for 1.7 and later.
/// * `shared_secret` — the 16-byte shared secret, before encryption.
/// * `public_der` — the server's public key exactly as it was sent in the
///   Encryption Request packet (the DER `SubjectPublicKeyInfo` bytes).
///
/// The result is the lowercase hexadecimal representation of the SHA-1 digest
/// interpreted as a signed big-endian integer; negative values are rendered
/// with a leading `-`. No leading zeros are emitted.
pub fn server_id_hash(server_id_initial: &str, shared_secret: &[u8], public_der: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(server_id_initial.as_bytes());
    hasher.update(shared_secret);
    hasher.update(public_der);
    let digest: [u8; 20] = hasher.finalize().into();
    signed_big_endian_hex(&digest)
}

/// Render a digest as the protocol's non-standard hexdigest: signed
/// two's-complement big-endian integer, lowercase base 16, no leading zeros.
fn signed_big_endian_hex(digest: &[u8; 20]) -> String {
    if digest[0] & 0x80 == 0 {
        return strip_leading_zeros(&hex_encode(digest));
    }
    // negative value: two's-complement negate the whole 160-bit array to get
    // its magnitude, then prepend '-'
    let mut magnitude = *digest;
    let mut carry = 1u16;
    for byte in magnitude.iter_mut().rev() {
        let value = (!*byte as u16) + carry;
        *byte = value as u8;
        carry = value >> 8;
    }
    format!("-{}", strip_leading_zeros(&hex_encode(&magnitude)))
}

fn hex_encode(bytes: &[u8; 20]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(40);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn strip_leading_zeros(hex: &str) -> String {
    let stripped = hex.trim_start_matches('0');
    if stripped.is_empty() {
        "0".to_owned()
    } else {
        stripped.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // the three worked examples from the Encryption page; with an empty
    // secret and no key bytes the hash input is just the ASCII string,
    // which makes these a direct check of the digest rendering
    #[test]
    fn wiki_example_notch() {
        assert_eq!(
            server_id_hash("Notch", &[], &[]),
            "4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48"
        );
    }

    #[test]
    fn wiki_example_jeb() {
        assert_eq!(
            server_id_hash("jeb_", &[], &[]),
            "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1"
        );
    }

    #[test]
    fn wiki_example_simon() {
        // digest starts with 0x08, exercising the positive leading-zero strip
        assert_eq!(
            server_id_hash("simon", &[], &[]),
            "88e16a1019277b15d58faf0541e11910eb756f6"
        );
    }

    #[test]
    fn golden_vector_empty_server_id() {
        // matches 1.7+ behavior: empty server id, 0x42 repeated secret,
        // DER-looking prefix followed by bytes 0..=255
        let mut public_der = vec![
            0x30, 0x82, 0x01, 0x22, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d,
            0x01, 0x01, 0x01, 0x05, 0x00, 0x03, 0x82, 0x01, 0x0f, 0x00,
        ];
        public_der.extend(0u8..=255);
        let secret = [0x42; 16];
        assert_eq!(
            server_id_hash("", &secret, &public_der),
            "-61708c00bee660b03b27c3bf80c6d1926209079b"
        );
    }

    #[test]
    fn golden_vector_named_server_id() {
        let secret: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let public_der = b"candyriya golden vector der bytes";
        assert_eq!(
            server_id_hash("candyriya-vector", &secret, public_der),
            "-44cf9b7b5d966a1c923fdf8099f2e2152036be2a"
        );
    }

    #[test]
    fn all_zero_digest_renders_as_zero() {
        // synthetic check of the strip-to-"0" path through the public fn
        assert_eq!(signed_big_endian_hex(&[0u8; 20]), "0");
    }

    #[test]
    fn most_negative_digest_renders_correctly() {
        // 0x80 0x00...0x00 is i160::MIN in two's complement
        let mut digest = [0u8; 20];
        digest[0] = 0x80;
        assert_eq!(
            signed_big_endian_hex(&digest),
            "-8000000000000000000000000000000000000000"
        );
    }
}
