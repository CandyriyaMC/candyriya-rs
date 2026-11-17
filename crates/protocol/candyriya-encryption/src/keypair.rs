//! The RSA keypair the server generates at startup for the online-mode
//! handshake.

use rsa::pkcs8::EncodePublicKey;
use rsa::{Pkcs1v15Encrypt, PrivateKey, PublicKey};

use crate::error::EncryptionError;
use crate::rng;

/// Width of a freshly generated keypair. Vanilla uses 1024 bits, but the
/// protocol page notes that longer keys do not break official clients, and
/// 2048 is the sane floor.
const KEY_BITS: usize = 2048;

/// An RSA keypair used to bootstrap the online-mode handshake.
///
/// `private` is the server's long-lived key (vanilla generates one at
/// startup). `public_der` is the ASN.1 DER encoding of the public key in
/// `SubjectPublicKeyInfo` (X.509) form — the exact bytes that are sent in the
/// Encryption Request packet and fed into the session-server `serverId` hash.
/// It is *not* a PKCS#1 encoding and *not* a full certificate.
///
/// The struct does not implement `Debug` on purpose: the private key stays
/// out of accidental log output. Zeroizing the key material itself is
/// handled by `rsa` on drop.
pub struct EncryptionKeypair {
    /// The server's RSA-2048 private key.
    pub private: PrivateKey,
    /// The public key as DER-encoded `SubjectPublicKeyInfo` (X.509). This is
    /// the on-the-wire format for the Encryption Request packet.
    pub public_der: Vec<u8>,
}

impl EncryptionKeypair {
    /// Generate a fresh 2048-bit keypair.
    ///
    /// Key generation is CPU-heavy; call it once at startup, not per
    /// connection.
    pub fn generate() -> Result<Self, EncryptionError> {
        let private = PrivateKey::new(&mut rng::thread_rng(), KEY_BITS)
            .map_err(EncryptionError::KeyGeneration)?;
        let public_der = private
            .to_public_key()
            .to_public_key_der()
            .map_err(EncryptionError::PublicKeyDer)?
            .as_bytes()
            .to_vec();
        Ok(Self {
            private,
            public_der,
        })
    }

    /// Encrypt `plaintext` with the public key, PKCS#1 v1.5 padding
    /// (RFC 8017 §7.2.1).
    ///
    /// The input is limited to `key_size - 11` bytes (245 bytes for a
    /// 2048-bit key); larger inputs fail with [`EncryptionError::Encryption`].
    /// Each call pads with fresh randomness, so equal plaintexts produce
    /// different ciphertexts.
    pub fn encrypt_with_public(&self, plaintext: &[u8]) -> Result<Vec<u8>, EncryptionError> {
        PublicKey::from(&self.private)
            .encrypt(&mut rng::thread_rng(), Pkcs1v15Encrypt, plaintext)
            .map_err(EncryptionError::Encryption)
    }

    /// Decrypt `ciphertext` with the private key, expecting PKCS#1 v1.5
    /// padding.
    ///
    /// Blinding is passed explicitly (rsa is built without its `getrandom`
    /// feature; see `rng`), so the private operation never runs unblinded.
    ///
    /// Note: since rsa 0.10, PKCS#1 v1.5 uses implicit rejection — invalid
    /// padding no longer returns an error. A tampered ciphertext (or one
    /// encrypted to a different key) decrypts to a deterministic *synthetic*
    /// plaintext instead. Callers must treat the returned bytes as
    /// attacker-controlled: online-mode authenticates them implicitly
    /// (the session-server `serverId` hash will not match a synthetic
    /// shared secret, so `hasJoined` fails), and offline-mode fails at
    /// the first AES-CFB8-decrypted packet. Only length/format violations
    /// come back as [`EncryptionError::Decryption`].
    pub fn decrypt_with_private(&self, ciphertext: &[u8]) -> Result<Vec<u8>, EncryptionError> {
        self.private
            .decrypt_blinded(&mut rng::thread_rng(), Pkcs1v15Encrypt, ciphertext)
            .map_err(EncryptionError::Decryption)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET_LEN: usize = 16;

    #[test]
    fn keygen_and_roundtrip_16_bytes() {
        let keypair = EncryptionKeypair::generate().expect("keygen should succeed");
        // the size of a shared secret plus verify token, padded to the key width
        let plaintext = [0x42; SECRET_LEN];
        let ciphertext = keypair
            .encrypt_with_public(&plaintext)
            .expect("encrypt should succeed");
        assert_eq!(ciphertext.len(), 256);
        assert_ne!(ciphertext[..SECRET_LEN], plaintext[..]);
        let decrypted = keypair
            .decrypt_with_private(&ciphertext)
            .expect("decrypt should succeed");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn keygen_and_roundtrip_144_bytes() {
        let keypair = EncryptionKeypair::generate().expect("keygen should succeed");
        // a full Encryption Response payload is two RSA-encrypted blobs;
        // 144 bytes is a realistic upper bound for one of them
        let plaintext: Vec<u8> = (0u8..144).collect();
        let ciphertext = keypair
            .encrypt_with_public(&plaintext)
            .expect("encrypt should succeed");
        let decrypted = keypair
            .decrypt_with_private(&ciphertext)
            .expect("decrypt should succeed");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn tampered_ciphertext_does_not_yield_original_plaintext() {
        let keypair = EncryptionKeypair::generate().expect("keygen should succeed");
        let mut ciphertext = keypair
            .encrypt_with_public(b"verify-token-verify-token!")
            .expect("encrypt should succeed");
        // break the leading 0x00 0x02 padding marker
        ciphertext[0] ^= 0x01;
        // rsa 0.10 applies implicit rejection: invalid padding returns a
        // deterministic synthetic plaintext, not an error. What is guaranteed
        // is that the original bytes do NOT come back — and that the output
        // is stable for the same (key, ciphertext) pair.
        let first = keypair
            .decrypt_with_private(&ciphertext)
            .expect("implicit rejection never errors here");
        let second = keypair
            .decrypt_with_private(&ciphertext)
            .expect("implicit rejection never errors here");
        assert_ne!(first, b"verify-token-verify-token!");
        assert_eq!(first, second);
    }

    #[test]
    fn wrong_key_does_not_yield_original_plaintext() {
        let alice = EncryptionKeypair::generate().expect("keygen should succeed");
        let bob = EncryptionKeypair::generate().expect("keygen should succeed");
        let ciphertext = alice
            .encrypt_with_public(b"for alice only")
            .expect("encrypt should succeed");
        // same implicit-rejection semantics: no error, but no secret either
        let synthetic = bob
            .decrypt_with_private(&ciphertext)
            .expect("implicit rejection never errors here");
        assert_ne!(synthetic, b"for alice only");
    }

    #[test]
    fn short_ciphertext_still_errors() {
        let keypair = EncryptionKeypair::generate().expect("keygen should succeed");
        // shorter than the 2048-bit modulus is the one hard length failure
        assert!(keypair.decrypt_with_private(b"tiny").is_err());
    }

    #[test]
    fn oversized_plaintext_fails() {
        let keypair = EncryptionKeypair::generate().expect("keygen should succeed");
        // 256 - 11 bytes is the PKCS#1 v1.5 ceiling
        let plaintext = vec![0u8; 246];
        assert!(keypair.encrypt_with_public(&plaintext).is_err());
    }

    #[test]
    fn public_der_is_stable_encoding_of_the_public_key() {
        let keypair = EncryptionKeypair::generate().expect("keygen should succeed");
        // SPKI for RSA-2048 starts with this SEQUENCE/OID prefix
        assert_eq!(&keypair.public_der[..2], &[0x30, 0x82]);
        assert_eq!(keypair.public_der.len(), 294);
    }
}
