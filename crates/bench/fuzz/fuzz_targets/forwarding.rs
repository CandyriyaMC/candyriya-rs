//! Fuzz the modern forwarding payload verifier.
//!
//! `verify_forwarding_data` is what a backend does with the bytes a proxy
//! hands it after the login plugin pull. The whole point of the format is
//! that a hostile payload is rejected *before* it reaches the field parser;
//! this target pushes arbitrary bytes at both sides of that boundary.
//!
//! Two input classes are exercised per run:
//!
//! * raw bytes with a fixed secret — almost everything dies at the HMAC
//!   check, which is what must happen for unauthenticated junk;
//! * the same bytes sealed with the fixed secret — the signature passes, so
//!   the field parser itself runs on attacker-controlled input, where a
//!   panic or an over-read would live;
//! * a secret/payload split derived from the input — both arguments arbitrary.
//!
//! A clean `Err` is a perfectly good outcome; a panic is not.

#![no_main]

use candyriya_forwarding::verify_forwarding_data;
use hmac::{Hmac, Mac};
use libfuzzer_sys::fuzz_target;
use sha2::Sha256;

const SECRET: &[u8] = b"candyriya-fuzz-forwarding-secret";

/// Seal `payload` with `secret` the same way the proxy does: the 32-byte
/// HMAC-SHA256 signature first, then the bytes it authenticates. Duplicated
/// here (instead of calling `create_forwarding_data`) because the fuzzer,
/// not the encoder API, picks the payload bytes.
fn seal(secret: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
        .expect("HMAC-SHA256 accepts keys of any length");
    mac.update(payload);
    let signature = mac.finalize().into_bytes();
    let mut sealed = Vec::with_capacity(signature.len() + payload.len());
    sealed.extend_from_slice(&signature);
    sealed.extend_from_slice(payload);
    sealed
}

fn utf16_units(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

fuzz_target!(|data: &[u8]| {
    // raw, unauthenticated input: must be rejected, never panic
    let _ = verify_forwarding_data(SECRET, data);

    // sealed input: the signature authenticates, so the parser runs on
    // arbitrary bytes
    let sealed = seal(SECRET, data);
    if let Ok((ip, profile)) = verify_forwarding_data(SECRET, &sealed) {
        // the IP came through read_string, so it must be UTF-8
        assert!(std::str::from_utf8(&ip).is_ok());
        // the account name is capped at 16 UTF-16 code units on the wire
        assert!(utf16_units(&profile.name) <= 16);
        // property strings went through read_string too
        for property in &profile.properties {
            assert!(utf16_units(&property.name) <= candyriya_protocol::DEFAULT_MAX_STRING_LENGTH);
            assert!(utf16_units(&property.value) <= candyriya_protocol::DEFAULT_MAX_STRING_LENGTH);
        }
    }

    // both arguments derived from the input: split point from the first byte
    if let Some((&hint, rest)) = data.split_first() {
        let split = (hint as usize) % (rest.len() + 1);
        let (secret, payload) = rest.split_at(split);
        let _ = verify_forwarding_data(secret, payload);
        // and once more with the payload sealed under the derived secret,
        // so the parser path runs with arbitrary secrets too
        let sealed = seal(secret, payload);
        let _ = verify_forwarding_data(secret, &sealed);
    }
});
