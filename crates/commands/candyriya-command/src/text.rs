//! Text helpers for embedding dynamic data into MiniMessage strings.

/// Escapes the two characters MiniMessage treats as markup delimiters.
///
/// Player- and server-supplied names are attacker-controlled text; MiniMessage
/// happily parses `<red>` into a color tag, so anything interpolated into a
/// MiniMessage template must pass through this first. The replacement is
/// U+2039/U+203A (single guillemets) — visually close to `<`/`>`, invisible to
/// the parser.
///
/// Mirrors `candyriya-plugin-api`'s helper of the same name; kept local so the
/// command crate does not depend on the plugin ABI crate for one pure
/// function.
#[must_use]
pub fn escape_minimessage(input: &str) -> String {
    input.replace('<', "‹").replace('>', "›")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_angle_brackets() {
        assert_eq!(escape_minimessage("a<b>c"), "a‹b›c");
        assert_eq!(escape_minimessage("<red>pwned</red>"), "‹red›pwned‹/red›");
        assert_eq!(escape_minimessage("plain"), "plain");
        assert_eq!(escape_minimessage(""), "");
    }
}
