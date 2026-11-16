//! Integration tests for the embedded locales and the loader.
//!
//! The embedded TOML files are the source of truth and are included here
//! directly, so these tests fail at build time if the files disappear and at
//! run time if they go out of sync or stop parsing as MiniMessage.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use candyriya_i18n::{I18n, LoadError};

const EN_COMMON: &str = include_str!("../embedded/en/common.toml");
const EN_CANDYRIYA: &str = include_str!("../embedded/en/candyriya.toml");
const EX_GIRL_COMMON: &str = include_str!("../embedded/ex_girl/common.toml");
const EX_GIRL_CANDYRIYA: &str = include_str!("../embedded/ex_girl/candyriya.toml");

fn load_toml(raw: &str) -> BTreeMap<String, String> {
    toml::from_str(raw).expect("embedded or overridden locale file must parse as TOML")
}

/// A temporary directory that is removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("candyriya-i18n-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write_file(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().expect("file has a parent"))
            .expect("create parent dirs");
        std::fs::write(&path, content).expect("write locale file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Tracing writer that captures everything written into `buf`.
#[derive(Clone)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capture_warns() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(CaptureWriter(Arc::clone(&buf)))
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (buf, guard)
}

fn captured_text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(buf.lock().unwrap_or_else(|p| p.into_inner()).clone())
        .expect("captured log is utf-8")
}

/// Check that every `<tag>` in `value` is a tag lobsterchat recognizes, and
/// return the plain text with all tags stripped.
///
/// lobsterchat silently stops parsing at the first unknown tag, so this is
/// how we *prove* a value fully parses instead of "it didn't crash".
fn minimessage_plain_text(value: &str) -> Result<String, String> {
    const FORMATTING: &[&str] = &[
        "obfuscated",
        "bold",
        "strikethrough",
        "underline",
        "italic",
        "reset",
    ];
    const NAMED_COLORS: &[&str] = &[
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

    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    loop {
        match rest.find(['<', '>']) {
            None => {
                out.push_str(rest);
                return Ok(out);
            }
            Some(i) => {
                let (before, at) = rest.split_at(i);
                out.push_str(before);
                if at.starts_with('>') {
                    return Err(format!("stray '>' in {value:?}"));
                }
                let close = at
                    .find('>')
                    .ok_or_else(|| format!("unterminated '<' tag in {value:?}"))?;
                let inner = &at[1..close];
                let known = FORMATTING.contains(&inner)
                    || inner
                        .strip_prefix('/')
                        .is_some_and(|t| FORMATTING.contains(&t))
                    || NAMED_COLORS.contains(&inner)
                    || inner.strip_prefix('#').is_some_and(|hex| {
                        !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit())
                    });
                if !known {
                    return Err(format!("unknown MiniMessage tag <{inner}> in {value:?}"));
                }
                rest = &at[close + 1..];
            }
        }
    }
}

#[test]
fn embedded_locales_load() {
    let en = I18n::new("en").expect("en must load");
    let ex_girl = I18n::new("ex_girl").expect("ex_girl must load");
    assert_eq!(en.locale(), "en");
    assert_eq!(ex_girl.locale(), "ex_girl");
    // both locales answer a real key from the common file
    assert!(!en.t("common.yes", &[]).is_empty());
    assert!(!ex_girl.t("common.yes", &[]).is_empty());
}

#[test]
fn en_and_ex_girl_keys_match_exactly() {
    for (file_en, file_ex) in [
        (&EN_COMMON, &EX_GIRL_COMMON),
        (&EN_CANDYRIYA, &EX_GIRL_CANDYRIYA),
    ] {
        let en_keys: HashSet<_> = load_toml(file_en).into_keys().collect();
        let ex_keys: HashSet<_> = load_toml(file_ex).into_keys().collect();
        let only_en: Vec<_> = en_keys.difference(&ex_keys).collect();
        let only_ex: Vec<_> = ex_keys.difference(&en_keys).collect();
        assert!(
            only_en.is_empty() && only_ex.is_empty(),
            "key mismatch: only-en {only_en:?}, only-ex-girl {only_ex:?}"
        );
    }
}

#[test]
fn every_embedded_value_parses_as_minimessage() {
    for (name, raw) in [
        ("en/common.toml", EN_COMMON),
        ("en/candyriya.toml", EN_CANDYRIYA),
        ("ex_girl/common.toml", EX_GIRL_COMMON),
        ("ex_girl/candyriya.toml", EX_GIRL_CANDYRIYA),
    ] {
        for (key, value) in load_toml(raw) {
            let expected =
                minimessage_plain_text(&value).unwrap_or_else(|err| panic!("{name} {key}: {err}"));
            let mut component = lobsterchat::message::lobster(&value);
            let actual = component.flatten();
            assert_eq!(
                actual, expected,
                "{name} {key}: lobsterchat parsed a different text than expected"
            );
        }
    }
}

#[test]
fn override_file_wins_over_embedded() {
    let tmp = TempDir::new("override-wins");
    tmp.write_file(
        "locales/ex_girl/candyriya.toml",
        "\"candyriya.proxy.listening\" = \"Custom listener on {address}\"\n",
    );
    let i18n = I18n::with_override_dir("ex_girl", &tmp.path().join("locales"))
        .expect("override dir is valid");
    assert_eq!(
        i18n.t("candyriya.proxy.listening", &[("address", "0.0.0.0:25565")]),
        "Custom listener on 0.0.0.0:25565"
    );
}

#[test]
fn broken_override_file_falls_back_file_wise() {
    let tmp = TempDir::new("broken-file-wise");
    // a file that cannot possibly parse
    tmp.write_file(
        "locales/ex_girl/candyriya.toml",
        "this is not = [valid toml",
    );
    // a valid sibling file in the same override tree
    tmp.write_file("locales/ex_girl/common.toml", "\"common.yes\" = \"Ага\"\n");

    let i18n = I18n::with_override_dir("ex_girl", &tmp.path().join("locales"))
        .expect("a broken file must not fail the whole load");

    // broken candyriya.toml -> embedded ex_girl default served
    let embedded = load_toml(EX_GIRL_CANDYRIYA);
    let expected = embedded["candyriya.proxy.listening"].replace("{address}", "x");
    assert_eq!(
        i18n.t("candyriya.proxy.listening", &[("address", "x")]),
        expected
    );
    // valid common.toml -> override served
    assert_eq!(i18n.t("common.yes", &[]), "Ага");
    // file-wise override replaces the whole common.toml, so the rest of the
    // embedded ex_girl common file is gone; t falls through to en/common.toml
    assert_eq!(i18n.t("common.no", &[]), "No");
}

#[test]
fn broken_override_file_falls_back_with_warning() {
    let tmp = TempDir::new("broken-warn");
    tmp.write_file("locales/en/common.toml", "not toml at all [");
    let (buf, _guard) = capture_warns();
    let _i18n = I18n::with_override_dir("en", &tmp.path().join("locales"))
        .expect("a broken file must not fail the whole load");
    let logs = captured_text(&buf);
    assert!(
        logs.contains("common.toml"),
        "warning must name the offending file, got: {logs}"
    );
    assert!(
        logs.contains("falling back"),
        "warning must say we fall back, got: {logs}"
    );
}

#[test]
fn active_locale_falls_back_to_en_for_missing_keys() {
    let tmp = TempDir::new("active-fallback-en");
    // valid TOML with exactly one key: everything else must come from en
    tmp.write_file("locales/ex_girl/common.toml", "\"common.yes\" = \"Ага\"\n");
    tmp.write_file(
        "locales/ex_girl/candyriya.toml",
        "\"candyriya.proxy.listening\" = \"Кастом <gold>{address}</gold>\"\n",
    );
    let i18n =
        I18n::with_override_dir("ex_girl", &tmp.path().join("locales")).expect("valid override");

    // override wins for its own key
    assert_eq!(i18n.t("common.yes", &[]), "Ага");
    // key missing from the override -> falls through to en/common.toml
    assert_eq!(i18n.t("common.no", &[]), "No");
    // same for candyriya.toml
    assert_eq!(
        i18n.t("candyriya.motd.example", &[]),
        load_toml(EN_CANDYRIYA)["candyriya.motd.example"]
    );
}

#[test]
fn reload_picks_up_file_changes() {
    let tmp = TempDir::new("reload-changes");
    let locales = tmp.path().join("locales");
    tmp.write_file(
        "locales/en/candyriya.toml",
        "\"candyriya.proxy.listening\" = \"V1 {address}\"\n",
    );

    let i18n = I18n::with_override_dir("en", &locales).expect("valid override");
    assert_eq!(
        i18n.t("candyriya.proxy.listening", &[("address", "a")]),
        "V1 a"
    );

    // swap the file contents; no new I18n is created
    tmp.write_file(
        "locales/en/candyriya.toml",
        "\"candyriya.proxy.listening\" = \"V2 {address}\"\n",
    );
    i18n.reload().expect("reload must succeed");
    assert_eq!(
        i18n.t("candyriya.proxy.listening", &[("address", "b")]),
        "V2 b"
    );
}

#[test]
fn reload_without_override_dir_is_a_noop() {
    let i18n = I18n::new("ex_girl").expect("embedded loads");
    i18n.reload()
        .expect("reload on embedded-only handle must succeed");
    assert_eq!(i18n.locale(), "ex_girl");
    assert!(!i18n.t("common.yes", &[]).is_empty());
}

#[test]
fn missing_key_returns_key_and_warns_once() {
    let (buf, _guard) = capture_warns();
    let i18n = I18n::new("en").expect("embedded loads");
    assert_eq!(i18n.t("totally.made.up.key", &[]), "totally.made.up.key");
    assert_eq!(i18n.t("totally.made.up.key", &[]), "totally.made.up.key");
    let logs = captured_text(&buf);
    assert!(
        logs.contains("totally.made.up.key"),
        "warn must name the key: {logs}"
    );
    assert_eq!(
        logs.matches("missing locale key").count(),
        1,
        "missing key must warn at most once per key: {logs}"
    );
}

#[test]
fn interpolation_named_repeated_positional() {
    let tmp = TempDir::new("interpolation");
    tmp.write_file(
        "locales/en/candyriya.toml",
        "\"candyriya.test.line\" = \"Hello {name}, bye {name}, pos {0}/{1}, dupe {dup}, unknown {mystery}, lone {unused}\"\n\"candyriya.test.unterminated\" = \"oops {name\"\n",
    );
    let i18n = I18n::with_override_dir("en", &tmp.path().join("locales")).expect("valid override");

    let out = i18n.t(
        "candyriya.test.line",
        &[
            ("name", "Bob"),
            ("0", "zero"),
            ("1", "one"),
            ("dup", "a"),
            ("dup", "b"),
            ("unused", "zzz"),
        ],
    );
    assert_eq!(
        out,
        "Hello Bob, bye Bob, pos zero/one, dupe b, unknown {mystery}, lone zzz"
    );

    // unknown tokens survive, unterminated brace is copied verbatim
    assert_eq!(i18n.t("candyriya.test.unterminated", &[]), "oops {name");
    assert_eq!(
        i18n.t("candyriya.test.line", &[]),
        "Hello {name}, bye {name}, pos {0}/{1}, dupe {dup}, unknown {mystery}, lone {unused}"
    );
}

#[test]
fn args_without_tokens_are_ignored_and_do_not_panic() {
    let i18n = I18n::new("en").expect("embedded loads");
    assert_eq!(i18n.t("common.yes", &[("ghost", "boo")]), "Yes");
}

#[test]
fn register_plugin_locale_end_to_end() {
    let tmp = TempDir::new("plugin-e2e");
    tmp.write_file(
        "plugin/ex_girl/myplugin.toml",
        "\"myplugin.only_active\" = \"Из плагина, {thing}\"\n\"myplugin.both\" = \"Плагин выиграл, {thing}\"\n\"common.yes\" = \"Казалось бы да\"\n",
    );
    tmp.write_file(
        "plugin/en/myplugin.toml",
        "\"myplugin.only_en\" = \"From plugin, {thing}\"\n\"myplugin.both\" = \"Plugin won, {thing}\"\n",
    );

    let i18n = I18n::new("ex_girl").expect("embedded loads");
    i18n.register_plugin_locale("myplugin", &tmp.path().join("plugin"))
        .expect("plugin registers");

    // active-locale plugin file wins
    assert_eq!(
        i18n.t("myplugin.only_active", &[("thing", "x")]),
        "Из плагина, x"
    );
    // en plugin file fills in keys missing from the active one
    assert_eq!(
        i18n.t("myplugin.only_en", &[("thing", "y")]),
        "From plugin, y"
    );
    // plugin file sits before common in the chain
    assert_eq!(
        i18n.t("myplugin.both", &[("thing", "z")]),
        "Плагин выиграл, z"
    );
    // plugin file position beats common.toml for its own keys
    assert_eq!(i18n.t("common.yes", &[]), "Казалось бы да");

    // re-register with new contents replaces the old table
    tmp.write_file(
        "plugin/ex_girl/myplugin.toml",
        "\"myplugin.only_active\" = \"Новая версия, {thing}\"\n",
    );
    i18n.register_plugin_locale("myplugin", &tmp.path().join("plugin"))
        .expect("re-register replaces");
    assert_eq!(
        i18n.t("myplugin.only_active", &[("thing", "w")]),
        "Новая версия, w"
    );
}

#[test]
fn reload_keeps_plugin_registrations() {
    let tmp = TempDir::new("reload-plugin");
    tmp.write_file("plugin/en/myplugin.toml", "\"myplugin.k\" = \"original\"\n");

    let i18n = I18n::new("en").expect("embedded loads");
    i18n.register_plugin_locale("myplugin", &tmp.path().join("plugin"))
        .expect("plugin registers");
    tmp.write_file("plugin/en/myplugin.toml", "\"myplugin.k\" = \"replaced\"\n");
    i18n.reload().expect("reload must succeed");
    assert_eq!(i18n.t("myplugin.k", &[]), "replaced");
}

#[test]
fn plugin_locale_missing_files_are_fine() {
    let tmp = TempDir::new("plugin-missing");
    // directory exists but holds no matching files
    std::fs::create_dir_all(tmp.path().join("plugin/en")).expect("create dir");
    let i18n = I18n::new("ex_girl").expect("embedded loads");
    i18n.register_plugin_locale("myplugin", &tmp.path().join("plugin"))
        .expect("missing plugin files are not an error");
    // plugin contributes nothing, common chain still works
    assert_eq!(i18n.t("common.yes", &[]), "Да");
}

#[test]
fn invalid_plugin_ids_are_rejected() {
    let i18n = I18n::new("en").expect("embedded loads");
    let long = "x".repeat(65);
    for bad in [
        "",
        "../escape",
        "..",
        "has space",
        "UPPER",
        "a/b",
        "a\\b",
        "-leading",
        "_leading",
        long.as_str(),
    ] {
        let result = i18n.register_plugin_locale(bad, Path::new("."));
        assert!(
            matches!(result, Err(LoadError::InvalidPluginId { .. })),
            "plugin id {bad:?} must be rejected"
        );
    }
    // a perfectly valid id works even if the directory does not exist yet
    let tmp = TempDir::new("plugin-valid");
    i18n.register_plugin_locale("my.plugin-1_ok", &tmp.path().join("nope"))
        .expect("valid id with missing dir must not fail");
}

#[test]
fn unknown_locales_are_rejected() {
    let err = I18n::new("de").expect_err("de is not a known locale");
    assert!(matches!(err, LoadError::UnknownLocale { .. }));
    let tmp = TempDir::new("unknown-locale");
    let err = I18n::with_override_dir("de", tmp.path()).expect_err("de is not a known locale");
    assert!(matches!(err, LoadError::UnknownLocale { .. }));
    let err = I18n::new("EN").expect_err("locale ids are case sensitive");
    assert!(matches!(err, LoadError::UnknownLocale { .. }));
}

#[test]
fn unreadable_override_root_is_an_error() {
    let tmp = TempDir::new("root-not-dir");
    // a regular file where a directory is expected
    let file = tmp.write_file("not-a-dir.toml", "x = 1");
    let err = I18n::with_override_dir("en", &file).expect_err("a file is not an override dir");
    assert!(matches!(err, LoadError::Io { .. }));
}

#[test]
fn reload_keeps_state_consistent_on_error() {
    // a handle built on embedded defaults, then reload after the override
    // root turned into a file: the swap must not happen, tables stay usable.
    let tmp = TempDir::new("reload-error");
    let locales = tmp.path().join("locales");
    tmp.write_file(
        "locales/en/candyriya.toml",
        "\"candyriya.proxy.listening\" = \"keep me\"\n",
    );
    let i18n = I18n::with_override_dir("en", &locales).expect("valid override");

    // remove the override root and replace it with a file
    std::fs::remove_dir_all(&locales).expect("remove override root");
    std::fs::write(&locales, "x = 1").expect("replace with file");
    let result = i18n.reload();
    assert!(matches!(result, Err(LoadError::Io { .. })));
    // the previously swapped-in table is still valid
    assert_eq!(i18n.t("candyriya.proxy.listening", &[]), "keep me");
}
