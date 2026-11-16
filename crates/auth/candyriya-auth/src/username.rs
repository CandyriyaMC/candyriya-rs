//! Username validation, shared by both validators.
//!
//! The rule enforced here is the legacy Java Edition rule (1.8+): a name is
//! 3–16 characters long and consists of ASCII alphanumeric characters and
//! underscores. There is no case rule — `"Notch"` and `"notch"` are both
//! valid and are kept verbatim (Mojang's protocol is case-preserving).

use crate::AuthError;

/// The minimum length of a Java Edition username.
pub const MIN_USERNAME_LEN: usize = 3;
/// The maximum length of a Java Edition username.
pub const MAX_USERNAME_LEN: usize = 16;

/// Check `username` against the Java Edition rules.
///
/// Returns [`AuthError::InvalidUsername`] with a specific reason when the
/// name is empty, shorter than [`MIN_USERNAME_LEN`], longer than
/// [`MAX_USERNAME_LEN`], or contains anything outside `[A-Za-z0-9_]`.
///
/// ```
/// use candyriya_auth::validate_username;
///
/// assert!(validate_username("Notch").is_ok());
/// assert!(validate_username("Herobrine_3rd").is_ok());
/// assert!(validate_username("ab").is_err());
/// assert!(validate_username("name with spaces").is_err());
/// ```
pub fn validate_username(username: &str) -> Result<(), AuthError> {
    // ASCII-only, so the byte length is also the character count.
    if username.len() < MIN_USERNAME_LEN {
        return Err(AuthError::InvalidUsername(format!(
            "{username:?} is shorter than the minimum of {MIN_USERNAME_LEN} characters"
        )));
    }
    if username.len() > MAX_USERNAME_LEN {
        return Err(AuthError::InvalidUsername(format!(
            "{username:?} is longer than the maximum of {MAX_USERNAME_LEN} characters"
        )));
    }
    if let Some(byte) = username
        .bytes()
        .find(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
    {
        return Err(AuthError::InvalidUsername(format!(
            "{username:?} contains {:?}; only ASCII alphanumerics and underscores are allowed",
            byte as char
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_lengths() {
        assert!(validate_username("abc").is_ok());
        assert!(validate_username("0123456789abcdef").is_ok());
        assert!(validate_username("ab").is_err());
        assert!(validate_username("0123456789abcdefg").is_err());
    }

    #[test]
    fn empty_is_rejected() {
        assert!(validate_username("").is_err());
    }

    #[test]
    fn underscore_and_digits_and_mixed_case_are_accepted() {
        assert!(validate_username("Notch").is_ok());
        assert!(validate_username("_").is_err()); // too short, not an underscore problem
        assert!(validate_username("___").is_ok());
        assert!(validate_username("a_b_C_7").is_ok());
    }

    #[test]
    fn disallowed_characters_are_rejected() {
        for name in [
            "name with spaces",
            "name-with-dash",
            "na.me",
            "na,me",
            "na@me",
            "n\u{e4}me", // non-ascii
            "玩家名",
        ] {
            assert!(
                validate_username(name).is_err(),
                "{name:?} should be rejected"
            );
        }
    }
}
