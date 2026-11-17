//! Minimal JSON string writer.
//!
//! The status response is built by hand: the harness contains no JSON
//! serializer on purpose, so the only thing that can go wrong is string
//! escaping — and that is concentrated in this one audited function. The
//! integration tests parse the produced document with `serde_json`
//! (a dev-dependency) and round-trip every field, hostile MOTDs included.

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Push `value` enclosed in double quotes, with every character JSON
/// forbids escaped: the two string delimiters and control characters below
/// U+0020. Multi-byte UTF-8 passes through unchanged — it is valid JSON.
pub(crate) fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u00");
                out.push(HEX[((c as u32) >> 4) as usize] as char);
                out.push(HEX[((c as u32) & 0x0f) as usize] as char);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escaped(value: &str) -> String {
        let mut out = String::new();
        push_json_string(&mut out, value);
        out
    }

    #[test]
    fn passes_plain_utf8_through() {
        assert_eq!(escaped("hello world € 𝄞"), "\"hello world € 𝄞\"");
    }

    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(
            escaped("say \"hi\" \\ once"),
            "\"say \\\"hi\\\" \\\\ once\""
        );
    }

    #[test]
    fn escapes_control_characters() {
        assert_eq!(escaped("a\nb\rc\td"), "\"a\\nb\\rc\\td\"");
        assert_eq!(escaped("\u{08}\u{0c}\u{1f}"), "\"\\b\\f\\u001f\"");
    }

    #[test]
    fn produces_parseable_json_string() {
        let nasty = "quotes\" backslash\\ ctrl\u{01} tab\t €\"\\";
        let doc = format!("{{\"k\":{}}}", escaped(nasty));
        let parsed: serde_json::Value = serde_json::from_str(&doc).expect("valid json");
        assert_eq!(parsed["k"], nasty);
    }
}
