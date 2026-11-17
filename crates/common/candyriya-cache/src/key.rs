//! Key validation matching the rules cachegirl enforces on resource names.
//!
//! cachegirl validates resource names internally but does not export that
//! check, so the proxy mirrors it here and the `cache` unit rejects bad keys
//! before they ever reach the backend. The accept/reject boundary is the
//! same one cachegirl applies: a key cachegirl would reject, this rejects;
//! a key cachegirl would accept, this accepts. The reasons are short and
//! static so a denial can cross the IPC boundary without owning a string.

/// Reserved device stems, case-insensitive, with any extension attached
/// (`NUL.txt`, `con.log`, ...). Windows treats these basenames as devices;
/// over-rejecting beats an alias, the same tradeoff cachegirl makes.
const RESERVED_DEVICE_STEMS: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validate a cache entry key the same way cachegirl validates resource names.
///
/// Returns a short reason on rejection. The rules, in order: non-empty after
/// trimming and no leading/trailing whitespace; not `.` or `..`; no `/`, `\\`
/// or `:`; no control characters; no trailing dot; no reserved device stem.
///
/// # Errors
///
/// A short static reason describing the first rule the key violates.
pub fn validate_cache_key(key: &str) -> Result<(), &'static str> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err("the key is empty");
    }
    if trimmed != key {
        return Err("the key has leading or trailing whitespace");
    }
    if key == "." || key == ".." {
        return Err("the key is a path traversal segment");
    }
    if key.contains(['/', '\\', ':']) {
        return Err("the key contains a path separator or ':'");
    }
    if key.chars().any(char::is_control) {
        return Err("the key contains control characters");
    }
    if key.ends_with('.') {
        return Err("the key ends with a dot");
    }
    let stem = key.split('.').next().unwrap_or("");
    if RESERVED_DEVICE_STEMS.contains(&stem.to_ascii_uppercase().as_str()) {
        return Err("the key is a reserved device name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_cache_key;

    #[test]
    fn accepts_what_cachegirl_accepts() {
        for key in ["status.favicon", "a", "x-y_z.1", "a.b.c.d"] {
            assert!(validate_cache_key(key).is_ok(), "{key} should be valid");
        }
    }

    #[test]
    fn rejects_what_cachegirl_rejects() {
        for key in [
            "",
            "  ",
            " padded",
            "padded ",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a:b",
            "nul",
            "NUL.txt",
            "con.log",
            "COM1",
            "lpt9.png",
            "trailing.",
            "with\ttab",
            "with\nnewline",
            "with\0nul",
        ] {
            assert!(
                validate_cache_key(key).is_err(),
                "{key:?} should be rejected"
            );
        }
    }
}
