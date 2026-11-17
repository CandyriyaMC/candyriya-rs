//! Fuzz the length-prefixed UTF-8 string reader.
//!
//! `read_string` is called on every string field in the protocol, always
//! with hostile input. It must never index out of bounds, never trust a
//! declared byte length past the buffer, and never hand out a string that
//! violates the UTF-16 unit cap it was given.
//!
//! Layout of the input bytes:
//!
//! ```text
//! byte  0     extra cap probe, 0..=255 (in addition to the fixed caps)
//! bytes 1..   the blob fed to the reader
//! ```
//!
//! Fixed caps of 0 / 1 / 16 / 255 / the format default are tried alongside
//! the probe cap so both the tiny-cap and the no-cap paths see coverage.

#![no_main]

use candyriya_protocol::{DEFAULT_MAX_STRING_LENGTH, read_string};
use libfuzzer_sys::fuzz_target;

fn utf16_units(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

fuzz_target!(|data: &[u8]| {
    let Some((&probe_cap, blob)) = data.split_first() else {
        return;
    };

    for max_len in [
        0_usize,
        1,
        16,
        255,
        DEFAULT_MAX_STRING_LENGTH,
        probe_cap as usize,
    ] {
        let mut src = blob;
        if let Ok(value) = read_string(&mut src, max_len) {
            // the reader must never return a string that exceeds the cap
            assert!(utf16_units(&value) <= max_len);
            // and never more bytes than max_len * 3 on the wire
            assert!(value.len() <= max_len.saturating_mul(3));
        }
    }
});
