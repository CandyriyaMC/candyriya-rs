//! Validation for plugin ids.
//!
//! Plugin ids double as directory name segments (`cache/<id>`,
//! `temp/<id>`), as namespace path segments in cachegirl's registry and
//! as lookup keys for per-plugin configuration, so they stay deliberately
//! boring: lowercase ASCII, digits, `-`, `_` and `.`, at most 32 bytes.
//! Uppercase is rejected on purpose — `Foo` and `foo` must never end up
//! pointing at the same directory on a case-insensitive filesystem.

use crate::error::{Error, Result};

/// Longest accepted plugin id, in bytes (ASCII, so bytes == chars).
pub const MAX_PLUGIN_ID_LEN: usize = 32;

// Windows treats these stems as device names with any extension, which is
// why `nul.txt` is rejected too, not just `nul` (same rule cachegirl
// applies to namespace segments).
const RESERVED_DEVICE_STEMS: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validate a plugin id.
///
/// Accepts `[a-z0-9_.-]{1,32}` with three extra filesystem-safety rules:
/// the id must not start with or be a `.` segment, must not end with
/// `.` (Windows strips trailing dots, which would alias two plugin ids
/// onto one directory), and its stem before the first `.` must not be a
/// reserved device name (case-insensitive; ids are lowercase anyway).
///
/// ```
/// use candyriya_cache::validate_plugin_id;
///
/// assert!(validate_plugin_id("lobby").is_ok());
/// assert!(validate_plugin_id("my.plugin-2").is_ok());
/// assert!(validate_plugin_id("bad id").is_err());
/// assert!(validate_plugin_id("nul.txt").is_err());
/// ```
pub fn validate_plugin_id(id: &str) -> Result<()> {
    let reject = |reason: &'static str| Error::InvalidPluginId {
        id: id.to_owned(),
        reason,
    };

    if id.is_empty() {
        return Err(reject("must not be empty"));
    }
    if id.len() > MAX_PLUGIN_ID_LEN {
        return Err(reject("must be at most 32 bytes"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
    {
        return Err(reject(
            "only lowercase ASCII letters, digits and `-`, `_`, `.` are allowed",
        ));
    }
    // covers `.` and `..` too: anything starting with a dot is out
    if id.starts_with('.') {
        return Err(reject("must not start with `.` (hidden directory)"));
    }
    if id.ends_with('.') {
        return Err(reject(
            "must not end with `.` (Windows strips trailing dots)",
        ));
    }
    // charset is ASCII-only at this point, so the stem slice is on a boundary
    let stem = id.split('.').next().unwrap_or(id);
    if RESERVED_DEVICE_STEMS.contains(&stem.to_ascii_uppercase().as_str()) {
        return Err(reject("must not be a reserved device name"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_lowercase_ids() {
        for id in [
            "a",
            "lobby",
            "plugin-a",
            "plugin_a",
            "my.plugin.2",
            "0",
            "x".repeat(32).as_str(),
        ] {
            assert!(validate_plugin_id(id).is_ok(), "{id} should be valid");
        }
    }

    #[test]
    fn rejects_empty_and_too_long() {
        assert!(validate_plugin_id("").is_err());
        assert!(validate_plugin_id(&"x".repeat(33)).is_err());
    }

    #[test]
    fn rejects_uppercase_and_weird_charset() {
        for id in [
            "Lobby", "bad id", "bad/id", "bad\\id", "bad:id", "bad*id", "chérie",
        ] {
            assert!(validate_plugin_id(id).is_err(), "{id} should be rejected");
        }
    }

    #[test]
    fn rejects_dot_trickery() {
        for id in [".", "..", "name.", ".hidden"] {
            assert!(validate_plugin_id(id).is_err(), "{id} should be rejected");
        }
    }

    #[test]
    fn rejects_reserved_device_names_with_any_extension() {
        for id in [
            "con", "PRN", "prn", "aux.log", "nul.txt", "com1", "com9.bin", "lpt1", "lpt9.cfg",
        ] {
            assert!(validate_plugin_id(id).is_err(), "{id} should be rejected");
        }
        // lookalikes that are not device names are fine
        for id in ["com10", "com01", "lpt10", "console", "null"] {
            assert!(validate_plugin_id(id).is_ok(), "{id} should be valid");
        }
    }
}
