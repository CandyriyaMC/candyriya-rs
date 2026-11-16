//! Random token generation for ids and secrets.
//!
//! Backed by [`rand`]'s thread-local RNG (ChaCha12 seeded from the OS), which is
//! the sanctioned source of randomness for the workspace. Uniqueness is not
//! guaranteed for any fixed length — it's just very likely; treat tokens as
//! probabilistically unique.

use rand::RngCore;
use rand::distr::{Alphanumeric, SampleString};

/// Generate `len_bytes` cryptographically strong random bytes.
///
/// # Examples
///
/// ```
/// use candyriya_utils::random_token;
///
/// let token = random_token(32);
/// assert_eq!(token.len(), 32);
/// ```
#[must_use]
pub fn random_token(len_bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len_bytes];
    rand::rng().fill_bytes(&mut buf);
    buf
}

/// Generate a random ASCII alphanumeric string of `len` characters.
///
/// Each character is uniformly drawn from `[a-zA-Z0-9]` (62 symbols). Handy for
/// forwarding secrets, rotation tokens and similar shared-secret material that
/// has to be typed or pasted by hand.
///
/// # Examples
///
/// ```
/// use candyriya_utils::random_token_alphanumeric;
///
/// let token = random_token_alphanumeric(32);
/// assert_eq!(token.len(), 32);
/// assert!(token.chars().all(|c| c.is_ascii_alphanumeric()));
/// ```
#[must_use]
pub fn random_token_alphanumeric(len: usize) -> String {
    Alphanumeric.sample_string(&mut rand::rng(), len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn random_token_has_requested_length() {
        assert_eq!(random_token(0).len(), 0);
        assert_eq!(random_token(1).len(), 1);
        assert_eq!(random_token(64).len(), 64);
    }

    #[test]
    fn random_tokens_are_unique_in_practice() {
        // 1024 draws of 16 bytes — collision probability is astronomically low
        let set: HashSet<Vec<u8>> = (0..1024).map(|_| random_token(16)).collect();
        assert_eq!(set.len(), 1024);
    }

    #[test]
    fn alphanumeric_has_requested_length_and_alphabet() {
        let token = random_token_alphanumeric(256);
        assert_eq!(token.len(), 256);
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(random_token_alphanumeric(0), "");
    }

    #[test]
    fn alphanumeric_tokens_are_unique_in_practice() {
        let set: HashSet<String> = (0..1024).map(|_| random_token_alphanumeric(32)).collect();
        assert_eq!(set.len(), 1024);
    }
}
