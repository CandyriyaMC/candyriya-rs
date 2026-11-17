//! The 16-byte shared secret used to bootstrap the AES-CFB8 session.

use zeroize::Zeroize;

/// A 16-byte (128-bit) shared secret.
///
/// The secret is the *only* key material of a session: it is used as the
/// AES-128 key and, with AES-CFB8, also as the IV. Both sides derive it
/// independently — the server receives it encrypted with its RSA public key.
///
/// Session secrets are authentication material; never cache, persist or log
/// them. The buffer is zeroized when the value is dropped.
///
/// `Debug` is implemented redacted so the secret cannot leak into logs.
pub struct SharedSecret(pub(crate) [u8; 16]);

impl SharedSecret {
    /// Length of the secret in bytes.
    pub const LEN: usize = 16;

    /// Generate a fresh secret from the given RNG.
    pub fn random(rng: &mut impl rand::Rng) -> Self {
        let mut bytes = [0u8; Self::LEN];
        rng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// Borrow the raw secret bytes (for example, to derive the AES key/IV).
    pub fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }

    /// Wrap an existing 16-byte array.
    ///
    /// The encryption handshake exists to *receive* a session secret from
    /// the client: the proxy RSA-decrypts the Encryption Response and
    /// reconstructs the shared secret from the decrypted bytes. This is the
    /// only legitimate way to build one; the secret is never persisted.
    pub fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for SharedSecret {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for SharedSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl core::fmt::Debug for SharedSecret {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SharedSecret([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_bytes_roundtrips_verbatim() {
        let bytes = [0x42; SharedSecret::LEN];
        let secret = SharedSecret::from_bytes(bytes);
        assert_eq!(secret.as_bytes(), &bytes);
    }

    #[test]
    fn random_secrets_are_16_bytes_and_distinct() {
        let mut rng = rand::rng();
        let a = SharedSecret::random(&mut rng);
        let b = SharedSecret::random(&mut rng);
        assert_eq!(a.as_bytes().len(), SharedSecret::LEN);
        assert_ne!(a.as_bytes(), b.as_bytes());
    }

    #[test]
    fn as_ref_leaks_nothing_but_slice() {
        let secret = SharedSecret([0x42; 16]);
        let slice: &[u8] = secret.as_ref();
        assert_eq!(slice, [0x42; 16]);
    }

    #[test]
    fn debug_output_is_redacted() {
        let secret = SharedSecret([0x42; 16]);
        let rendered = format!("{secret:?}");
        assert_eq!(rendered, "SharedSecret([REDACTED])");
        assert!(!rendered.contains("66"));
    }
}
