//! AES-128-CFB8, the stream cipher of the encrypted session.
//!
//! The protocol page is explicit about two things people get wrong: the
//! feedback size is 8 bits (1 byte), and the cipher state is *not* reset per
//! packet. Both directions are updated continuously across all packets. Key
//! and IV are both set to the 16-byte shared secret.

use aes::Aes128;
use aes::cipher::{Iv, Key, KeyIvInit};
use cfb8::{Decryptor, Encryptor};

use crate::shared_secret::SharedSecret;

type Enc = Encryptor<Aes128>;
type Dec = Decryptor<Aes128>;

/// The encryption/decryption cipher pair of one encrypted session.
///
/// The two directions are independent instances with independent feedback
/// state, mirroring what vanilla does. The struct is stateful by design:
/// reusing one instance across the whole session keeps the CFB feedback
/// chain intact. Creating a fresh pair mid-session desynchronizes the
/// stream.
pub struct AesCfb8 {
    encryptor: Enc,
    decryptor: Dec,
}

impl AesCfb8 {
    /// Create the cipher pair. Key and IV are both the shared secret bytes.
    pub fn new(secret: &SharedSecret) -> Self {
        let key = Key::<Enc>::from(*secret.as_bytes());
        let iv = Iv::<Enc>::from(*secret.as_bytes());
        let dec_key = Key::<Dec>::from(*secret.as_bytes());
        let dec_iv = Iv::<Dec>::from(*secret.as_bytes());
        Self {
            encryptor: Enc::new(&key, &iv),
            decryptor: Dec::new(&dec_key, &dec_iv),
        }
    }

    /// Encrypt `data` in place. A stream cipher does not change the length,
    /// so the buffer keeps its size.
    pub fn encrypt_in_place(&mut self, data: &mut [u8]) {
        self.encryptor.encrypt(data);
    }

    /// Decrypt `data` in place. The buffer keeps its size.
    pub fn decrypt_in_place(&mut self, data: &mut [u8]) {
        self.decryptor.decrypt(data);
    }
}

impl core::fmt::Debug for AesCfb8 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // the feedback state is key material, keep it out of logs
        f.write_str("AesCfb8([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(len: usize) -> Vec<u8> {
        (0..len as u32).map(|i| (i * 37 % 251) as u8).collect()
    }

    #[test]
    fn roundtrip_full_and_trailing_partial_blocks() {
        let mut rng = rand::rng();
        let secret = SharedSecret::random(&mut rng);
        // CFB8 has no block boundary at all; the interesting sizes are
        // around the AES block width and around arbitrary lengths
        for len in [1usize, 7, 15, 16, 17, 31, 32, 33, 64, 100, 256] {
            let plaintext = sample(len);
            let mut enc = AesCfb8::new(&secret);
            let mut dec = AesCfb8::new(&secret);

            let mut wire = plaintext.clone();
            enc.encrypt_in_place(&mut wire);
            assert_eq!(wire.len(), plaintext.len());
            assert_ne!(wire, plaintext);

            dec.decrypt_in_place(&mut wire);
            assert_eq!(wire, plaintext, "roundtrip failed at len {len}");
        }
    }

    #[test]
    fn chunked_stream_matches_one_shot() {
        let mut rng = rand::rng();
        let secret = SharedSecret::random(&mut rng);
        let plaintext = sample(48);

        let mut one_shot = AesCfb8::new(&secret);
        let mut expected = plaintext.clone();
        one_shot.encrypt_in_place(&mut expected);

        // encrypt the same input in two chunks through one session
        let mut chunked = AesCfb8::new(&secret);
        let mut head = plaintext[..20].to_vec();
        let mut tail = plaintext[20..].to_vec();
        chunked.encrypt_in_place(&mut head);
        chunked.encrypt_in_place(&mut tail);
        let actual: Vec<u8> = head.into_iter().chain(tail).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn decryptor_state_is_independent_of_encryptor() {
        let mut rng = rand::rng();
        let secret = SharedSecret::random(&mut rng);
        // encrypting first must not disturb the decryptor's feedback chain
        let mut session = AesCfb8::new(&secret);
        let mut scratch = sample(32);
        session.encrypt_in_place(&mut scratch);

        let mut send = AesCfb8::new(&secret);
        let mut receive = AesCfb8::new(&secret);
        let payload = sample(24);
        let mut wire = payload.clone();
        send.encrypt_in_place(&mut wire);
        receive.decrypt_in_place(&mut wire);
        assert_eq!(wire, payload);
    }

    #[test]
    fn wrong_secret_does_not_roundtrip() {
        let mut rng = rand::rng();
        let secret_a = SharedSecret::random(&mut rng);
        let secret_b = SharedSecret::random(&mut rng);
        let plaintext = sample(32);
        let mut enc = AesCfb8::new(&secret_a);
        let mut dec = AesCfb8::new(&secret_b);
        let mut wire = plaintext.clone();
        enc.encrypt_in_place(&mut wire);
        dec.decrypt_in_place(&mut wire);
        assert_ne!(wire, plaintext);
    }

    #[test]
    fn empty_input_is_a_noop() {
        let secret = SharedSecret([0x42; 16]);
        let mut session = AesCfb8::new(&secret);
        let mut data = Vec::new();
        session.encrypt_in_place(&mut data);
        session.decrypt_in_place(&mut data);
        assert!(data.is_empty());
    }
}
