//! All failures that can occur while using this crate.

use thiserror::Error;

/// Everything that can go wrong while generating keys, exchanging them or
/// encoding them.
///
/// Data-level failures on the peer side (malformed `Encryption Response`, for
/// example) surface through [`EncryptionError::Decryption`]; they are
/// normal protocol events, not bugs, and callers are expected to disconnect
/// the offending connection instead of panicking. Note that since rsa 0.10
/// PKCS#1 v1.5 uses implicit rejection, so *bad padding* does not raise this
/// error — it yields a deterministic synthetic plaintext instead (see
/// `keypair::EncryptionKeypair::decrypt_with_private`).
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum EncryptionError {
    /// The RSA key generation failed. Practically only happens when the
    /// system RNG is broken.
    #[error("failed to generate RSA keypair: {0}")]
    KeyGeneration(rsa::Error),

    /// Encrypting with the public key failed. The most common cause is a
    /// plaintext larger than what the key can hold (key size minus 11 bytes
    /// for PKCS#1 v1.5 padding).
    #[error("failed to encrypt with the public key: {0}")]
    Encryption(rsa::Error),

    /// Decrypting with the private key failed: the ciphertext was malformed
    /// (too short for the modulus) or the operation itself failed. Bad
    /// padding no longer surfaces here since rsa 0.10 implicit rejection —
    /// it returns synthetic plaintext, not an error.
    #[error("failed to decrypt with the private key: {0}")]
    Decryption(rsa::Error),

    /// Encoding the public key as DER failed. Serialization into a byte
    /// buffer cannot run out of space in a meaningful way, so this is
    /// effectively unreachable, but it is modeled instead of panicking.
    #[error("failed to encode the public key as DER: {0}")]
    PublicKeyDer(#[source] rsa::pkcs8::spki::Error),
}
