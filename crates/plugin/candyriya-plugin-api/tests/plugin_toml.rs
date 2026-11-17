//! Integration tests: parse real `plugin.toml` documents against the
//! contract types, validate them, and round-trip them.

use candyriya_plugin_api::{ApiInfo, MetadataError, PluginMetadata, ProxyVersion};

const MINIMAL_TOML: &str = r#"
id = "core"
version = "0.1.0"
name = "Core"
plugin_main = "plugin_main"
plugin_api = "0.1"
"#;

#[test]
fn parses_full_fixture() {
    let raw = include_str!("fixtures/minimal_plugin.toml");
    let meta: PluginMetadata = toml::from_str(raw).expect("fixture must parse");

    assert_eq!(meta.id, "example-plugin");
    assert_eq!(meta.version, "1.4.0");
    assert_eq!(meta.name, "Example Plugin");
    assert_eq!(
        meta.description.as_deref(),
        Some("Demonstrates the plugin ABI contract with every knob turned on.")
    );
    assert_eq!(meta.authors.len(), 2);
    assert_eq!(
        meta.website.as_deref(),
        Some("https://example.com/example-plugin")
    );
    assert_eq!(meta.license.as_deref(), Some("GPL-3.0-only"));
    assert_eq!(meta.plugin_main, "example_plugin_main");
    assert_eq!(meta.plugin_api, "0.1");
    assert_eq!(meta.depend, vec!["example-library"]);
    assert_eq!(meta.softdepend, vec!["another-library"]);
    assert_eq!(meta.permissions, vec!["players.read", "servers.switch"]);
    assert_eq!(meta.cache_quota_bytes, Some(67_108_864));
    assert_eq!(meta.cache_quota_resources, Some(1024));
    assert_eq!(meta.api.exports.len(), 2);
    assert_eq!(meta.api.exports[0].name, "greet");
    assert_eq!(meta.api.exports[1].name, "status");
    assert!(meta.locales.is_some());

    meta.validate().expect("fixture must validate");
}

#[test]
fn parses_required_fields_only() {
    let meta: PluginMetadata = toml::from_str(MINIMAL_TOML).expect("minimal manifest must parse");

    assert_eq!(meta.id, "core");
    assert_eq!(meta.description, None);
    assert!(meta.authors.is_empty());
    assert_eq!(meta.website, None);
    assert_eq!(meta.license, None);
    assert!(meta.depend.is_empty());
    assert!(meta.softdepend.is_empty());
    assert!(meta.permissions.is_empty());
    assert_eq!(meta.cache_quota_bytes, None);
    assert_eq!(meta.cache_quota_resources, None);
    assert_eq!(meta.api, ApiInfo::default());
    assert_eq!(meta.locales, None);

    meta.validate().expect("minimal manifest must validate");
}

#[test]
fn missing_required_field_is_an_error() {
    let result = toml::from_str::<PluginMetadata>("id = \"core\"\n");
    assert!(result.is_err());
}

#[test]
fn unknown_keys_are_tolerated_for_forward_compat() {
    let raw = format!("{MINIMAL_TOML}\nfuture_field = \"from a newer host\"\n");
    let meta: PluginMetadata = toml::from_str(&raw).expect("unknown keys must not break parsing");
    meta.validate()
        .expect("unknown keys must not break validation");
}

#[test]
fn validation_reports_every_problem_at_once() {
    let raw = r#"
id = "Invalid ID"
version = "not-semver"
name = "   "
plugin_main = "0bad name"
plugin_api = "1"
depend = ["valid-dep", "Invalid Dep"]
softdepend = ["valid-dep", "also bad!"]
permissions = ["ok", "ok", " padded "]
[api]
exports = [{ name = "has space" }, { name = "dupe" }, { name = "dupe" }]
"#;
    let meta: PluginMetadata = toml::from_str(raw).expect("structurally valid toml");
    let err = meta.validate().expect_err("must fail validation");

    let MetadataError::Validation(issues) = err else {
        panic!("expected Validation variant");
    };
    for expected in [
        "id",
        "version",
        "name",
        "plugin_main",
        "plugin_api",
        "depend[1]",
        "softdepend[1]",
        "both depend and softdepend",
        "duplicate \"ok\"", // duplicate permission
        "permissions[2]",   // padded
        "api.exports[0]",
        "duplicate \"dupe\"",
    ] {
        assert!(
            issues.contains(expected),
            "expected issue containing {expected:?}, got: {issues}"
        );
    }
}

#[test]
fn website_must_be_http() {
    let raw = MINIMAL_TOML.replace("plugin_api", "website = \"ftp://example.com\"\nplugin_api");
    let meta: PluginMetadata = toml::from_str(&raw).expect("must parse");
    let err = meta.validate().expect_err("ftp website must be rejected");
    assert!(err.to_string().contains("website"));
}

#[test]
fn toml_roundtrip_preserves_metadata() {
    let raw = include_str!("fixtures/minimal_plugin.toml");
    let meta: PluginMetadata = toml::from_str(raw).expect("fixture must parse");
    let serialized = toml::to_string_pretty(&meta).expect("must serialize");
    let back: PluginMetadata = toml::from_str(&serialized).expect("must re-parse");
    assert_eq!(meta, back);
}

#[test]
fn plugin_api_compatibility_gate() {
    let raw = include_str!("fixtures/minimal_plugin.toml");
    let meta: PluginMetadata = toml::from_str(raw).expect("fixture must parse");
    assert!(ProxyVersion::current().is_api_compatible(&meta.plugin_api));

    // a plugin built for a future minor refuses to load on this host
    assert!(!ProxyVersion::current().is_api_compatible("0.2"));
}
