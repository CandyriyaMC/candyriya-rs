//! Minimal MiniMessage → terminal renderer for console output.
//!
//! The command layer emits MiniMessage lines (the i18n tables are MiniMessage
//! by contract). This module renders those lines for the terminal: ANSI SGR
//! sequences when colors are enabled, plain text otherwise. The supported
//! grammar is exactly the strict gate's — named colors, hex colors and the
//! formatting tags plus `<reset>`; click/hover decoration is dropped, there
//! is no mouse on a machine console.
//!
//! Lines that fail the strict gate are printed raw with terminal control
//! characters removed: output must never be lost to a parse error, and a
//! config file or plugin string must never inject escape sequences into a
//! terminal.

use candyriya_plugin_api::parse_minimessage;
use serde_json::Value;

/// Render `line` for the terminal.
///
/// `ansi` selects ANSI SGR emission; when `false` the same text is produced
/// with every style dropped, which is what pipes and `NO_COLOR` want.
#[must_use]
pub fn render(line: &str, ansi: bool) -> String {
    let component = parse_minimessage(line)
        .ok()
        .and_then(|component| serde_json::to_value(&component).ok());
    match component {
        Some(value) => {
            let mut out = String::with_capacity(line.len() + 32);
            let mut last = Style::default();
            let mut emitted = false;
            walk(
                &value,
                &Style::default(),
                ansi,
                &mut last,
                &mut emitted,
                &mut out,
            );
            if ansi && emitted {
                out.push_str("\x1b[0m");
            }
            out
        }
        None => sanitize_raw(line),
    }
}

/// Effective text style of a component node, inherited down the tree.
#[derive(Clone, Default, PartialEq, Eq)]
struct Style {
    /// Raw SGR parameter for the foreground (`31`, `38;2;r;g;b`), if any.
    color: Option<String>,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    /// No ANSI equivalent; rendered as inverse video, which is what most
    /// terminals do with `reverse`.
    obfuscated: bool,
}

impl Style {
    fn is_plain(&self) -> bool {
        self.color.is_none()
            && !self.bold
            && !self.italic
            && !self.underline
            && !self.strikethrough
            && !self.obfuscated
    }

    fn sgr(&self) -> String {
        let mut params: Vec<&str> = Vec::with_capacity(6);
        if let Some(color) = &self.color {
            params.push(color);
        }
        if self.bold {
            params.push("1");
        }
        if self.italic {
            params.push("3");
        }
        if self.underline {
            params.push("4");
        }
        if self.strikethrough {
            params.push("9");
        }
        if self.obfuscated {
            params.push("7");
        }
        format!("\x1b[{}m", params.join(";"))
    }
}

fn walk(
    value: &Value,
    parent: &Style,
    ansi: bool,
    last: &mut Style,
    emitted: &mut bool,
    out: &mut String,
) {
    let Some(map) = value.as_object() else {
        return;
    };

    let mut style = parent.clone();
    if map.get("reset").and_then(Value::as_bool) == Some(true) {
        style = Style::default();
    }
    if let Some(color) = map.get("color") {
        style.color = color_sgr(color);
    }
    for (key, apply) in [
        ("bold", Style::set_bold as fn(&mut Style, bool)),
        ("italic", Style::set_italic),
        ("underlined", Style::set_underline),
        ("strikethrough", Style::set_strikethrough),
        ("obfuscated", Style::set_obfuscated),
    ] {
        if let Some(value) = map.get(key).and_then(Value::as_bool) {
            apply(&mut style, value);
        }
    }

    if ansi && style != *last {
        if style.is_plain() {
            out.push_str("\x1b[0m");
        } else {
            out.push_str(&style.sgr());
        }
        *last = style.clone();
        *emitted = true;
    }

    if let Some(text) = map.get("text").and_then(Value::as_str) {
        push_sanitized(out, text);
    } else if let Some(translate) = map.get("translate").and_then(Value::as_str) {
        // i18n is resolved before rendering, so this only shows up if a
        // locale template itself carries a translate component
        push_sanitized(out, translate);
    } else if let Some(keybind) = map.get("keybind").and_then(Value::as_str) {
        push_sanitized(out, keybind);
    } else if map.contains_key("score") {
        out.push_str("<score>");
    } else if map.contains_key("selector") {
        out.push_str("<entity>");
    } else if map.contains_key("nbt") {
        out.push_str("<nbt>");
    }

    if let Some(children) = map.get("extra").and_then(Value::as_array) {
        for child in children {
            walk(child, &style, ansi, last, emitted, out);
        }
    }

    // restore the parent style so sibling nodes get their own formatting
    if ansi && *last != *parent {
        if parent.is_plain() {
            out.push_str("\x1b[0m");
        } else {
            out.push_str(&parent.sgr());
        }
        *last = parent.clone();
    }
}

impl Style {
    fn set_bold(&mut self, value: bool) {
        self.bold = value;
    }
    fn set_italic(&mut self, value: bool) {
        self.italic = value;
    }
    fn set_underline(&mut self, value: bool) {
        self.underline = value;
    }
    fn set_strikethrough(&mut self, value: bool) {
        self.strikethrough = value;
    }
    fn set_obfuscated(&mut self, value: bool) {
        self.obfuscated = value;
    }
}

/// Map a serialized lobsterchat `TextColor` to an SGR foreground parameter.
fn color_sgr(color: &Value) -> Option<String> {
    let raw = color.as_str()?;
    if let Some(hex) = raw.strip_prefix('#') {
        let (r, g, b) = match hex.len() {
            3 => {
                let mut nibbles = hex
                    .chars()
                    .map(|c| u8::from_str_radix(&c.to_string(), 16).ok());
                let (Some(r), Some(g), Some(b)) =
                    (nibbles.next()?, nibbles.next()?, nibbles.next()?)
                else {
                    return None;
                };
                (r * 17, g * 17, b * 17)
            }
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                (r, g, b)
            }
            _ => return None,
        };
        return Some(format!("38;2;{r};{g};{b}"));
    }
    named_color_sgr(raw).map(str::to_owned)
}

/// Standard 16-color terminal palette, deliberately the bright half for the
/// "normal" colors so output stays readable on dark and light themes.
fn named_color_sgr(name: &str) -> Option<&'static str> {
    match name {
        "black" => Some("30"),
        "dark_blue" => Some("34"),
        "dark_green" => Some("32"),
        "dark_aqua" => Some("36"),
        "dark_red" => Some("31"),
        "dark_purple" => Some("35"),
        "gold" => Some("33"),
        "gray" => Some("37"),
        "dark_gray" => Some("90"),
        "blue" => Some("94"),
        "green" => Some("92"),
        "aqua" => Some("96"),
        "red" => Some("91"),
        "light_purple" => Some("95"),
        "yellow" => Some("93"),
        "white" => Some("97"),
        _ => None,
    }
}

/// Append `text` with all terminal control characters removed.
fn push_sanitized(out: &mut String, text: &str) {
    for ch in text.chars() {
        let code = ch as u32;
        // keep printable ASCII and all real unicode; drop C0/DEL/C1 controls
        if (0x20..=0x7e).contains(&code) || code >= 0xa0 {
            out.push(ch);
        }
    }
}

/// Fallback for lines that fail the strict MiniMessage gate: the text with
/// terminal control characters removed.
fn sanitize_raw(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    push_sanitized(&mut out, line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_color_renders_sgr() {
        let out = render("<aqua>hello", true);
        assert!(out.contains("\x1b[96m"), "{out:?}");
        assert!(out.contains("hello"));
        assert!(out.ends_with("\x1b[0m"), "{out:?}");
    }

    #[test]
    fn plain_mode_strips_all_escapes() {
        let out = render("<aqua><bold>hi</bold>", false);
        assert_eq!(out, "hi");
    }

    #[test]
    fn hex_color_renders_truecolor_sgr() {
        let out = render("<#FF8000>hi", true);
        assert!(out.contains("\x1b[38;2;255;128;0m"), "{out:?}");
    }

    #[test]
    fn short_hex_expands() {
        let out = render("<#a0b>hi", true);
        assert!(out.contains("\x1b[38;2;170;0;187m"), "{out:?}");
    }

    #[test]
    fn reset_tag_clears_style() {
        let out = render("<aqua>one<reset>two", true);
        let one = out.find("one").expect("text present");
        let reset = out[one..].find("\x1b[0m").expect("reset emitted");
        assert!(reset < out[one..].find("two").unwrap_or(usize::MAX));
    }

    #[test]
    fn sibling_segments_have_independent_colors() {
        let out = render("<aqua>a <red>b", true);
        assert!(out.contains("\x1b[96m"), "{out:?}");
        assert!(out.contains("\x1b[91m"), "{out:?}");
    }

    #[test]
    fn malformed_minimessage_falls_back_to_raw_text() {
        // the strict gate rejects the line, so the raw text goes out
        // with only control characters removed — never a halved message
        let out = render("</gold>keep me", true);
        assert_eq!(out, "</gold>keep me");
    }

    #[test]
    fn control_characters_are_stripped() {
        let out = render("evil\x1b[2J text", false);
        assert!(!out.contains('\x1b'), "{out:?}");
        assert!(out.contains("evil[2J text"));
    }

    #[test]
    fn newlines_are_stripped_from_output() {
        let out = render("first\nsecond", false);
        assert_eq!(out, "firstsecond");
    }
}
