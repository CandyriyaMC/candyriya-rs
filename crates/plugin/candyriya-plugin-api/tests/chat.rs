//! Integration tests for the chat/message-level API: the component builder,
//! the kick/disconnect reasons and the MiniMessage acceptance gate.
//!
//! No network, no runtime: everything here is pure construction and
//! serialization.

use candyriya_plugin_api::{
    ClickEvent, ComponentBuilder, DisconnectReason, HoverEvent, KickReason, MessageMode,
    NamedColor, escape_minimessage, parse_minimessage, text, translatable,
};
use serde_json::json;

fn component_json(component: &lobsterchat::component::Component) -> serde_json::Value {
    serde_json::to_value(component).expect("component must serialize")
}

#[test]
fn literal_component_color_applied() {
    let component = ComponentBuilder::text("hello")
        .color(NamedColor::Red)
        .build();
    assert_eq!(component_json(&component)["color"], "red");
    let hex = ComponentBuilder::text("hello")
        .hex_color(0x12_34_AB)
        .build();
    assert_eq!(component_json(&hex)["color"], "#1234AB");
}

#[test]
fn formatting_flags_are_set() {
    let bold = ComponentBuilder::text("x").bold().build();
    assert!(bold.get_bold());
    let italic = ComponentBuilder::text("x").italic().build();
    assert!(italic.get_italic());
    let underlined = ComponentBuilder::text("x").underlined().build();
    assert!(underlined.get_underlined());
    let strikethrough = ComponentBuilder::text("x").strikethrough().build();
    assert!(strikethrough.get_strikethrough());
    let obfuscated = ComponentBuilder::text("x").obfuscated().build();
    assert!(obfuscated.get_obfuscated());
    // stacking formatting keeps everything set
    let stacked = ComponentBuilder::text("x").bold().italic().build();
    assert!(stacked.get_bold());
    assert!(stacked.get_italic());
    // bold overrides the default (unspecified) flag to explicit true
    let plain = text("x");
    assert!(!plain.get_bold());
    let overridden = ComponentBuilder::text("x").bold().build();
    assert!(overridden.get_bold());
}

#[test]
fn append_order_is_preserved() {
    let mut component = ComponentBuilder::text("foo")
        .append(text("bar"))
        .append(text("baz"))
        .build();
    let json = component_json(&component);
    let extra = json["extra"].as_array().expect("extra array");
    let children: Vec<&str> = extra
        .iter()
        .map(|c| c["text"].as_str().expect("literal child"))
        .collect();
    assert_eq!(children, ["bar", "baz"]);
    assert_eq!(component.flatten(), "foobarbaz");
}

#[test]
fn hover_event_is_applied() {
    let component = ComponentBuilder::text("hover me")
        .hover_event(HoverEvent::show_text(text("tip")))
        .build();
    let json = component_json(&component);
    assert_eq!(json["hoverEvent"]["action"], "show_text");
    assert_eq!(json["hoverEvent"]["contents"]["text"], "tip");
}

#[test]
fn click_event_is_applied() {
    let component = ComponentBuilder::text("click me")
        .click_event(ClickEvent::open_url("https://example.com"))
        .build();
    let json = component_json(&component);
    assert_eq!(
        json["clickEvent"],
        json!({"action": "open_url", "value": "https://example.com"})
    );
}

#[test]
fn insert_text_is_applied() {
    let component = ComponentBuilder::text("shift-click")
        .insert("/msg Alice")
        .build();
    assert_eq!(component_json(&component)["insertion"], "/msg Alice");
}

#[test]
fn separator_appends_a_reset_literal() {
    let mut component = ComponentBuilder::text("lobby").separator(" | ").build();
    let json = component_json(&component);
    let extra = json["extra"].as_array().expect("extra array");
    assert_eq!(extra[0]["text"], " | ");
    assert_eq!(extra[0]["reset"], true);
    assert_eq!(component.flatten(), "lobby | ");
}

#[test]
fn translatable_carries_arguments() {
    let mut component = translatable("multiplayer.player.joined", &[text("Alice")]);
    let json = component_json(&component);
    assert_eq!(json["translate"], "multiplayer.player.joined");
    assert_eq!(json["with"][0]["text"], "Alice");
    assert_eq!(component.flatten(), "multiplayer.player.joined");

    let no_args = translatable("gui.done", &[]);
    assert!(component_json(&no_args).get("with").is_none());
}

#[test]
fn component_survives_a_json_roundtrip() {
    let original = ComponentBuilder::text("round")
        .color(NamedColor::Aqua)
        .bold()
        .append(text("trip"))
        .build();
    let json = serde_json::to_string(&original).expect("serialize");
    let back: lobsterchat::component::Component = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, original);
}

#[test]
fn kick_reasons_render_non_empty_text() {
    let reasons = [
        KickReason::ServerFull,
        KickReason::Maintenance,
        KickReason::TransferUnavailable,
        KickReason::VersionMismatch,
        KickReason::NeverAuthenticated,
        KickReason::ForcedHost {
            host: "lobby.example.com".to_owned(),
        },
        KickReason::Shutdown,
        KickReason::Unreachable,
    ];
    for reason in &reasons {
        for locale in ["en", "ex_girl"] {
            let rendered = reason.to_kick_text(locale);
            assert!(
                !rendered.is_empty(),
                "{reason:?} must render non-empty in {locale}"
            );
            // and must be MiniMessage the rest of the proxy can parse
            assert!(
                parse_minimessage(&rendered).is_ok(),
                "{reason:?} in {locale} must parse as MiniMessage: {rendered:?}"
            );
        }
        assert!(!reason.raw_minimessage().is_empty());
    }
}

#[test]
fn forced_host_reason_interpolates_and_sanitizes_host() {
    let reason = KickReason::ForcedHost {
        host: "<gold>wherever</gold>.example.com".to_owned(),
    };
    let rendered = reason.to_kick_text("en");
    assert!(!rendered.contains("<gold>wherever"));
    assert!(rendered.contains("‹gold›wherever‹/gold›.example.com"));
    let clean = KickReason::ForcedHost {
        host: "clean.example.com".to_owned(),
    };
    assert!(clean.to_kick_text("en").contains("clean.example.com"));
}

#[test]
fn disconnect_reason_is_the_kick_reason_shape() {
    let alias: DisconnectReason = KickReason::Maintenance;
    assert_eq!(alias, KickReason::Maintenance);
}

#[test]
fn shared_i18n_handle_recovers_from_missing_key() {
    // en override file that keeps exactly one kick key: every other kick
    // key vanishes from the table, so the raw fallback must kick in
    // instead of leaking a bare key as a user-facing message.
    let dir = std::env::temp_dir().join(format!(
        "candyriya-plugin-api-chat-test-{}",
        std::process::id()
    ));
    let locales = dir.join("locales").join("en");
    std::fs::create_dir_all(&locales).expect("create override dir");
    std::fs::write(
        locales.join("candyriya.toml"),
        "\"candyriya.kick.maintenance\" = \"Custom maintenance\"\n",
    )
    .expect("write override");

    let i18n = candyriya_i18n::I18n::with_override_dir("en", &dir.join("locales"))
        .expect("override dir is valid");

    let maintenance = KickReason::Maintenance.to_kick_text_with(&i18n);
    assert_eq!(maintenance, "Custom maintenance");

    let full = KickReason::ServerFull.to_kick_text_with(&i18n);
    assert_eq!(full, KickReason::ServerFull.raw_minimessage());

    std::fs::remove_dir_all(&dir).expect("clean up");
}

#[test]
fn unknown_locale_falls_back_to_raw_english() {
    let reason = KickReason::Shutdown;
    assert_eq!(reason.to_kick_text("de"), reason.raw_minimessage());
}

#[test]
fn minimessage_acceptance_gate() {
    let ok = parse_minimessage("<red>Hello <bold>world<reset>!");
    assert!(ok.is_ok());

    for bad in [
        "</gold>",                  // color closing tags are not supported by lobsterchat
        "<rainbow>hi",              // unknown tag
        "<unclosed",                // unterminated
        "stray > bracket",          // stray '>'
        "<#XYZ>",                   // hex with non-hex digits
        "<#11223344AABB>",          // 12 hex digits: lobsterchat would silently fail too
        "<hover:show_text:['hi']>", // event tags are not supported either
    ] {
        assert!(parse_minimessage(bad).is_err(), "{bad:?} must be rejected");
    }
}

#[test]
fn escape_minimessage_neutralizes_untrusted_text() {
    assert_eq!(escape_minimessage("plain"), "plain");
    assert_eq!(
        escape_minimessage("<red>injected</red>"),
        "‹red›injected‹/red›"
    );
    // the escaped text must parse to plain literal contents, not tags
    let escaped = escape_minimessage("<bold>");
    let mut component = parse_minimessage(&escaped).expect("escaped text parses");
    assert_eq!(component.flatten(), "‹bold›");
}

#[test]
fn message_mode_has_stable_wire_names() {
    assert_eq!(MessageMode::Chat.as_str(), "chat");
    assert_eq!(MessageMode::System.as_str(), "system");
    assert_eq!(MessageMode::GameInfo.as_str(), "game_info");
    let json = serde_json::to_string(&MessageMode::GameInfo).expect("serialize");
    assert_eq!(json, "\"game_info\"");
}
