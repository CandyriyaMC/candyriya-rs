//! MiniMessage: the strict acceptance gate for plugin- and config-provided
//! MiniMessage strings.
//!
//! lobsterchat's parser never fails. When it meets a tag it does not know,
//! the lexer yields an error token, the parser stops at that point and
//! returns whatever component it has accumulated so far — the rest of the
//! message is silently dropped. (That is why the embedded locales are
//! written without e.g. closing color tags: `</gold>` is not a tag
//! lobsterchat knows, and the text after it would never be rendered.)
//!
//! For an ABI boundary that is not acceptable: a plugin author who typos
//! `<rainbow>` should get an error back, not a silently halved message.
//! [`parse_minimessage`] therefore rejects any input lobsterchat cannot
//! fully consume — anything except named colors, hex colors and the
//! formatting tags (and their closing forms) — and only then hands the
//! actual parse over to it.
//!
//! It also provides [`escape_minimessage`], for interpolating
//! untrusted strings (player names, forced-host addresses, ...) into
//! messages that will later be parsed as MiniMessage.

use lobsterchat::component::Component;

use crate::metadata::MetadataError;

/// The named colors lobsterchat (and this ABI) accept in MiniMessage input.
const NAMED_COLORS: [&str; 16] = [
    "dark_red",
    "red",
    "gold",
    "yellow",
    "dark_green",
    "green",
    "aqua",
    "dark_aqua",
    "dark_blue",
    "blue",
    "light_purple",
    "dark_purple",
    "white",
    "gray",
    "dark_gray",
    "black",
];

/// The formatting tags lobsterchat accepts, with or without a leading `/`.
/// Colors have no closing form on purpose: `</gold>` is not a tag and the
/// strict gate must reject it rather than let lobsterchat truncate there.
const FORMATTING_TAGS: [&str; 6] = [
    "obfuscated",
    "bold",
    "strikethrough",
    "underline",
    "italic",
    "reset",
];

/// Checks the inner text of a `<...>` tag against the tag grammar
/// lobsterchat can fully consume.
fn is_known_tag(inner: &str) -> bool {
    // closing tags: only the formatting tags have a closing form
    if let Some(name) = inner.strip_prefix('/') {
        return FORMATTING_TAGS.contains(&name);
    }
    if let Some(hex) = inner.strip_prefix('#') {
        // lobsterchat hex tags accept any run of hex digits, but the value
        // is parsed into a u32, so longer runs silently fail there too
        return !hex.is_empty() && hex.len() <= 8 && hex.bytes().all(|b| b.is_ascii_hexdigit());
    }
    NAMED_COLORS.contains(&inner) || FORMATTING_TAGS.contains(&inner)
}

/// Rejects any input that contains something lobsterchat cannot fully
/// parse: unterminated tags, stray `>` and tags outside the grammar
/// understood by [`parse_minimessage`].
///
/// Returns `Ok(())` when the input is guaranteed to survive a lobsterchat
/// parse without truncation.
pub(crate) fn validate_minimessage_input(input: &str) -> Result<(), MetadataError> {
    // '<' and '>' are ASCII and can never appear inside a multi-byte UTF-8
    // sequence (continuation bytes are >= 0x80), so a byte scan is safe and
    // every slice index below lands on a char boundary.
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => {
                let rest = &input[i..];
                let Some(close_rel) = rest.find('>') else {
                    return Err(MetadataError::MiniMessage {
                        reason: format!("unterminated tag starting at byte {i}"),
                    });
                };
                let inner = &rest[1..close_rel];
                if inner.is_empty() {
                    return Err(MetadataError::MiniMessage {
                        reason: format!("empty tag at byte {i}"),
                    });
                }
                if !is_known_tag(inner) {
                    return Err(MetadataError::MiniMessage {
                        reason: format!("unknown MiniMessage tag <{inner}> at byte {i}"),
                    });
                }
                i += close_rel + 1;
            }
            b'>' => {
                return Err(MetadataError::MiniMessage {
                    reason: format!("stray '>' at byte {i}"),
                });
            }
            _ => i += 1,
        }
    }
    Ok(())
}

/// Parses a MiniMessage string into a component, strictly.
///
/// This is the acceptance gate for MiniMessage coming from plugins or from
/// user-facing strings a plugin asked the host to send: anything lobsterchat
/// would silently truncate is rejected instead, with a reason that names the
/// offending tag and its byte offset.
///
/// Placeholder tags are not supported here — the strict grammar has no
/// placeholders, matching the fact that this function is not a templating
/// engine: interpolation happens host-side via [`crate::translatable`] or
/// before parsing.
///
/// # Examples
///
/// ```
/// use candyriya_plugin_api::parse_minimessage;
///
/// let ok = parse_minimessage("<red>Hello<reset>").expect("known tags parse");
/// assert!(parse_minimessage("</gold>").is_err());
/// assert!(parse_minimessage("<rainbow>hi</rainbow>").is_err());
/// ```
pub fn parse_minimessage(input: &str) -> Result<Component, MetadataError> {
    validate_minimessage_input(input)?;
    Ok(lobsterchat::lobster(input))
}

/// Escapes angle brackets so an untrusted string can be interpolated into a
/// message that will later be parsed as MiniMessage without turning the
/// string's contents into tags.
///
/// `<` becomes `‹` and `>` becomes `›`, the typography equivalents that no
/// MiniMessage implementation treats as tag delimiters. lobsterchat has no
/// escape mechanism, so this substitution is the only safe way to include
/// player-provided text in a MiniMessage string.
///
/// # Examples
///
/// ```
/// use candyriya_plugin_api::escape_minimessage;
///
/// assert_eq!(escape_minimessage("<gold>not a tag</gold>"), "‹gold›not a tag‹/gold›");
/// ```
pub fn escape_minimessage(input: &str) -> String {
    if !input.contains(['<', '>']) {
        return input.to_owned();
    }
    input.replace('<', "\u{2039}").replace('>', "\u{203a}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tags_are_accepted() {
        for ok in [
            "plain text",
            "<red>colored<reset>",
            "<#AAFF00>hex<reset>",
            "<#a>short hex",
            "<bold><italic><underline>",
            "<obfuscated><strikethrough><underline>",
            "</bold></italic></reset>",
            "</obfuscated></strikethrough></underline>",
            "",
            "<red>closed ok",
        ] {
            assert!(validate_minimessage_input(ok).is_ok(), "{ok:?} must parse");
        }
    }

    #[test]
    fn lobsterchat_truncations_are_rejected() {
        for bad in [
            "</gold>",
            "</red>",
            "<rainbow>x",
            "<hover:show_text:'hi'>x",
            "<click:run_command:'x'>x",
            "<gradient:red:blue>x",
            "<red has spaces>",
            "<#>",
            "<#XYZ123>",
            // 9 hex digits: must not pass the gate even though lobsterchat stops here too
            "<#11223344AABB>",
            "<",
            "<red",
            "<>",
            ">",
            "a > b",
            "<a<b>",
        ] {
            assert!(
                validate_minimessage_input(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn escape_replaces_angle_brackets_only() {
        assert_eq!(escape_minimessage(""), "");
        assert_eq!(escape_minimessage("plain"), "plain");
        assert_eq!(escape_minimessage("<b>"), "‹b›");
        assert_eq!(escape_minimessage("a<b>c<d"), "a‹b›c‹d");
    }
}
