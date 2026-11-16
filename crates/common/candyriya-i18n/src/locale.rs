//! Locale tables: embedded defaults, override directories, fallback chains,
//! and hot reload.
//!
//! The public handle is [`I18n`]; it is cheap to clone (everything inside is
//! `Arc`) and safe to share between threads.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;

use crate::error::LoadError;

/// The only locales this crate ships and the only identifiers accepted.
const LOCALES: [&str; 2] = ["en", "ex_girl"];

const EN_COMMON: &str = include_str!("../embedded/en/common.toml");
const EN_CANDYRIYA: &str = include_str!("../embedded/en/candyriya.toml");
const EX_GIRL_COMMON: &str = include_str!("../embedded/ex_girl/common.toml");
const EX_GIRL_CANDYRIYA: &str = include_str!("../embedded/ex_girl/candyriya.toml");

/// Parsed contents of one locale file: a flat key -> MiniMessage map.
type LocaleMap = HashMap<String, String>;

/// Parse an embedded locale file.
///
/// Embedded files ship with the crate and are exercised by the test suite, so
/// a parse failure here means the crate itself is broken. We still never
/// panic: degrade to an empty table and log, the en fallback will kick in.
fn embedded_map(name: &str, input: &str) -> LocaleMap {
    match toml::from_str::<LocaleMap>(input) {
        Ok(map) => map,
        Err(err) => {
            tracing::error!(locale_file = name, %err, "embedded locale file failed to parse");
            LocaleMap::new()
        }
    }
}

/// Outcome of reading one override file.
enum FileLoad {
    /// File exists and parsed.
    Map(LocaleMap),
    /// File does not exist (fine, it just does not override).
    Missing,
    /// File exists but could not be read or parsed; the caller falls back to
    /// embedded defaults for this file only.
    Broken,
}

/// Read `<dir>/<lang>/<file_name>` as a locale file.
///
/// Content problems are never fatal: they produce a warning and [`FileLoad::Broken`].
fn load_file(dir: &Path, lang: &str, file_name: &str) -> FileLoad {
    let path = dir.join(lang).join(file_name);
    match fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<LocaleMap>(&text) {
            Ok(map) => FileLoad::Map(map),
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "override locale file failed to parse; falling back to embedded defaults for this file");
                FileLoad::Broken
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => FileLoad::Missing,
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "override locale file unreadable; falling back to embedded defaults for this file");
            FileLoad::Broken
        }
    }
}

/// Read an override file, returning `None` when absent or broken (after
/// logging in the broken case).
fn override_file(dir: Option<&Path>, lang: &str, file_name: &str) -> Option<LocaleMap> {
    let dir = dir?;
    match load_file(dir, lang, file_name) {
        FileLoad::Map(map) => Some(map),
        FileLoad::Missing | FileLoad::Broken => None,
    }
}

/// Make sure an override root that the caller explicitly passed in is readable.
///
/// A missing root is fine: it simply means "embedded defaults everywhere".
/// Anything else that fails (not a directory, permission denied) is an error,
/// so `/candyriya reload` can surface it.
fn check_override_root(dir: &Path) -> Result<(), LoadError> {
    match fs::metadata(dir) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(LoadError::Io {
                    path: dir.to_path_buf(),
                    source: io::Error::new(
                        io::ErrorKind::NotADirectory,
                        "override path exists but is not a directory",
                    ),
                });
            }
            // metadata can succeed while the directory itself cannot be listed.
            fs::read_dir(dir).map_err(|source| LoadError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(LoadError::Io {
            path: dir.to_path_buf(),
            source,
        }),
    }
}

/// Fully resolved locale tables, built in one pass and swapped atomically
/// into place (initial load, `/candyriya reload`, plugin registration).
struct Locales {
    active_candyriya: LocaleMap,
    active_common: LocaleMap,
    en_candyriya: LocaleMap,
    en_common: LocaleMap,
    /// Registered plugin files, in registration order.
    plugins: Vec<PluginLocales>,
}

/// Locale file pair of one plugin, resolved for the build.
struct PluginLocales {
    active: LocaleMap,
    en: LocaleMap,
}

impl Locales {
    fn lookup(&self, key: &str) -> Option<&str> {
        self.active_candyriya
            .get(key)
            .or_else(|| self.plugins.iter().find_map(|p| p.active.get(key)))
            .or_else(|| self.en_candyriya.get(key))
            .or_else(|| self.plugins.iter().find_map(|p| p.en.get(key)))
            .or_else(|| self.active_common.get(key))
            .or_else(|| self.en_common.get(key))
            .map(String::as_str)
    }
}

/// Build a fresh, fully resolved [`Locales`] from embedded defaults, the
/// optional override directory, and the registered plugins.
fn build_locales(
    active: &str,
    override_dir: Option<&Path>,
    plugins: &[(String, PathBuf)],
) -> Locales {
    let en_candyriya = override_file(override_dir, "en", "candyriya.toml")
        .unwrap_or_else(|| embedded_map("en/candyriya.toml", EN_CANDYRIYA));
    let en_common = override_file(override_dir, "en", "common.toml")
        .unwrap_or_else(|| embedded_map("en/common.toml", EN_COMMON));

    let (active_candyriya, active_common) = if active == "en" {
        (en_candyriya.clone(), en_common.clone())
    } else {
        let candyriya = override_file(override_dir, active, "candyriya.toml")
            .unwrap_or_else(|| embedded_map("ex_girl/candyriya.toml", EX_GIRL_CANDYRIYA));
        let common = override_file(override_dir, active, "common.toml")
            .unwrap_or_else(|| embedded_map("ex_girl/common.toml", EX_GIRL_COMMON));
        (candyriya, common)
    };

    let plugins = plugins
        .iter()
        .map(|(id, dir)| {
            let file_name = format!("{id}.toml");
            let active_map = override_file(Some(dir), active, &file_name).unwrap_or_default();
            let en_map = if active == "en" {
                active_map.clone()
            } else {
                override_file(Some(dir), "en", &file_name).unwrap_or_default()
            };
            PluginLocales {
                active: active_map,
                en: en_map,
            }
        })
        .collect();

    Locales {
        active_candyriya,
        active_common,
        en_candyriya,
        en_common,
        plugins,
    }
}

/// Substitute `{token}` placeholders in `input`.
///
/// * `{name}` resolves against the `args` pairs. When several args share a
///   name, the last one wins.
/// * `{0}`, `{1}`, … resolve positionally into `args` by index.
/// * Unknown tokens are left exactly as they are (`{mystery}` stays
///   `{mystery}`).
/// * Args whose name never appears in the string are ignored.
/// * An unterminated `{` is copied verbatim, nothing is truncated silently.
fn interpolate(input: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(input.len().saturating_add(16));
    let mut i = 0;
    while i < input.len() {
        let slice = &input[i..];
        match slice.find('{') {
            None => {
                out.push_str(slice);
                break;
            }
            Some(open_rel) => {
                let open = i + open_rel;
                out.push_str(&input[i..open]);
                match input[open..].find('}') {
                    Some(close_rel) => {
                        let close = open + close_rel;
                        let token = &input[open + 1..close];
                        match resolve_token(token, args) {
                            Some(value) => out.push_str(value),
                            None => {
                                out.push('{');
                                out.push_str(token);
                                out.push('}');
                            }
                        }
                        i = close + 1;
                    }
                    None => {
                        // no closing brace: nothing more to interpolate
                        out.push_str(&input[open..]);
                        break;
                    }
                }
            }
        }
    }
    out
}

fn resolve_token<'a>(token: &str, args: &'a [(&str, &str)]) -> Option<&'a str> {
    if let Some((_, value)) = args.iter().rev().find(|(name, _)| *name == token) {
        return Some(*value);
    }
    token
        .parse::<usize>()
        .ok()
        .and_then(|index| args.get(index))
        .map(|(_, value)| *value)
}

fn warn_missing_once(missing: &Mutex<HashSet<String>>, key: &str) {
    let mut guard = missing.lock().unwrap_or_else(|p| p.into_inner());
    if guard.insert(key.to_owned()) {
        tracing::warn!(key, "missing locale key; returning the key itself");
    }
}

/// Check that `id` is safe to use as a file name (`<id>.toml`).
///
/// Plugin locale files are looked up as `<dir>/<lang>/<id>.toml`; an id like
/// `../../etc` would escape the plugin directory, so ids are restricted to a
/// lowercase dotted-name subset.
fn validate_plugin_id(id: &str) -> Result<(), LoadError> {
    let valid = match id.chars().next() {
        Some(first) => first.is_ascii_lowercase() || first.is_ascii_digit(),
        None => false,
    } && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'));

    if valid {
        Ok(())
    } else {
        Err(LoadError::InvalidPluginId { id: id.to_owned() })
    }
}

/// Reject anything that is not one of the built-in locales.
fn validate_active(active: &str) -> Result<(), LoadError> {
    if LOCALES.contains(&active) {
        Ok(())
    } else {
        Err(LoadError::UnknownLocale {
            locale: active.to_owned(),
        })
    }
}

/// Locale handle for the proxy and its plugins.
///
/// Cloning is cheap: everything inside is shared. The tables are swapped
/// atomically on [`I18n::reload`] and [`I18n::register_plugin_locale`], so
/// readers never observe a half-built table.
///
/// Build one with [`I18n::new`] (embedded defaults only) or
/// [`I18n::with_override_dir`] (embedded + data-directory overrides).
pub struct I18n {
    active: Arc<str>,
    override_dir: Option<Arc<Path>>,
    inner: Arc<ArcSwap<Locales>>,
    plugins: Arc<Mutex<Vec<(String, PathBuf)>>>,
    missing: Arc<Mutex<HashSet<String>>>,
}

impl fmt::Debug for I18n {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("I18n")
            .field("active", &self.active)
            .field("override_dir", &self.override_dir.as_deref())
            .finish_non_exhaustive()
    }
}

impl Clone for I18n {
    fn clone(&self) -> Self {
        Self {
            active: Arc::clone(&self.active),
            override_dir: self.override_dir.as_ref().map(Arc::clone),
            inner: Arc::clone(&self.inner),
            plugins: Arc::clone(&self.plugins),
            missing: Arc::clone(&self.missing),
        }
    }
}

impl I18n {
    /// Create a handle with the embedded locales only.
    ///
    /// `active` must be `"en"` or `"ex_girl"`; anything else is
    /// [`LoadError::UnknownLocale`].
    pub fn new(active: &str) -> Result<Self, LoadError> {
        Self::build(active, None)
    }

    /// Create a handle with the embedded locales overridden file-wise by
    /// `<dir>/<active>/common.toml`, `<dir>/<active>/candyriya.toml` (and the
    /// `en` siblings when the active locale is not English).
    ///
    /// Missing files are fine and simply keep the embedded defaults. A file
    /// that fails to read or parse is logged with `tracing::warn!` and the
    /// embedded defaults are used for that file only.
    ///
    /// The call returns an error only when `dir` exists but cannot be used as
    /// a directory at all (it is a file, or listing it fails); a `dir` that
    /// does not exist is equivalent to the embedded defaults. So content
    /// problems in locale files never fail, only the explicitly passed
    /// directory being unreadable does.
    pub fn with_override_dir(active: &str, dir: &Path) -> Result<Self, LoadError> {
        check_override_root(dir)?;
        Self::build(active, Some(dir))
    }

    fn build(active: &str, override_dir: Option<&Path>) -> Result<Self, LoadError> {
        validate_active(active)?;
        let plugins = Vec::new();
        let locales = build_locales(active, override_dir, &plugins);
        Ok(Self {
            active: Arc::from(active),
            override_dir: override_dir.map(Arc::from),
            inner: Arc::new(ArcSwap::from_pointee(locales)),
            plugins: Arc::new(Mutex::new(plugins)),
            missing: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    /// The active locale identifier (`"en"` or `"ex_girl"`).
    pub fn locale(&self) -> &str {
        &self.active
    }

    /// Look up `key` and interpolate `args`.
    ///
    /// The fallback chain is, in order:
    ///
    /// 1. `<active>/candyriya.toml`, then each registered plugin file (registration order);
    /// 2. `en/candyriya.toml`, then each registered plugin file;
    /// 3. `<active>/common.toml`;
    /// 4. `en/common.toml`;
    /// 5. the key itself, with a one-time `tracing::warn!` per key.
    ///
    /// Interpolation supports `{name}` (named args; last wins on duplicates),
    /// `{0}`, `{1}` (positional args), and repeated tokens. Unknown tokens are
    /// left as-is; args that are never referenced are ignored; an unterminated
    /// `{` is copied verbatim.
    ///
    /// This function never panics.
    pub fn t(&self, key: &str, args: &[(&str, &str)]) -> String {
        let value = {
            let table = self.inner.load();
            table.lookup(key).map(str::to_owned)
        };
        match value {
            Some(value) => interpolate(&value, args),
            None => {
                warn_missing_once(&self.missing, key);
                key.to_owned()
            }
        }
    }

    /// Re-read the override directory and all registered plugin directories,
    /// then atomically swap in the rebuilt tables.
    ///
    /// Called by `/candyriya reload`. When this handle was built with
    /// [`I18n::new`], the rebuilt tables are identical (embedded defaults) and
    /// the call is a no-op that still succeeds.
    ///
    /// Content problems in override files never fail the reload: the affected
    /// file falls back to its embedded (or, for plugins, empty) defaults with
    /// a warning. The call fails with [`LoadError::Io`] only when the
    /// override directory that was passed in no longer exists as a readable
    /// directory.
    pub fn reload(&self) -> Result<(), LoadError> {
        if let Some(dir) = &self.override_dir {
            check_override_root(dir)?;
        }
        let plugins = self
            .plugins
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let rebuilt = build_locales(&self.active, self.override_dir.as_deref(), &plugins);
        self.inner.store(Arc::new(rebuilt));
        // re-arm the missing-key warnings: a reload may have removed or added keys
        self.missing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        Ok(())
    }

    /// Register (or replace) a plugin's locale file in the fallback chain.
    ///
    /// Looks up `<dir>/<active>/<plugin_id>.toml` and, when the active locale
    /// is not English, `<dir>/en/<plugin_id>.toml`. These files are checked
    /// after the core `candyriya` file and before `common`, so a plugin can
    /// override shared strings for its own keys without touching the common
    /// files. Keys stay the plugin's own names.
    ///
    /// A file that is missing is fine; a file that fails to read or parse is
    /// logged with `tracing::warn!` and treated as absent. The call fails
    /// with [`LoadError::InvalidPluginId`] when `plugin_id` is not a safe
    /// file name, and with [`LoadError::Io`] when `dir` exists but cannot be
    /// used as a directory.
    ///
    /// Re-registering the same `plugin_id` replaces its file contents; the
    /// plugin moves to the end of the chain.
    pub fn register_plugin_locale(&self, plugin_id: &str, dir: &Path) -> Result<(), LoadError> {
        validate_plugin_id(plugin_id)?;
        check_override_root(dir)?;
        if !dir.exists() {
            tracing::warn!(path = %dir.display(), plugin_id, "plugin locale directory does not exist; plugin keys will fall back to common");
        }
        let snapshot = {
            let mut plugins = self.plugins.lock().unwrap_or_else(|p| p.into_inner());
            plugins.retain(|(id, _)| id != plugin_id);
            plugins.push((plugin_id.to_owned(), dir.to_path_buf()));
            plugins.clone()
        };
        let rebuilt = build_locales(&self.active, self.override_dir.as_deref(), &snapshot);
        self.inner.store(Arc::new(rebuilt));
        Ok(())
    }
}
