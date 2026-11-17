//! The shared locale machines plugin host functions resolve through.
//!
//! The proxy runs two i18n machines (`en` and `ex_girl`), each with the
//! embedded tables plus whatever overrides the data directory carries.
//! [`SharedLocales`] is the same pair, cloned: every clone shares the
//! underlying tables and the plugin-registration list, so a plugin
//! registration (or a [`candyriya_i18n::I18n::reload`] from
//! `/candyriya reload`) is visible to every host state instantly.
//!
//! Plugins never pick a machine; the host picks one based on the
//! plugin's *active* locale (`en` unless overridden), mirroring how the
//! command layer picks a per-invoker locale.

use std::path::Path;

use candyriya_i18n::{I18n, LoadError};

/// The pair of locale machines shared with the rest of the proxy.
///
/// Registration is expected to happen on **both** handles: the active-locale
/// handle serves `<active>/<plugin-id>.toml`, and the `en` handle serves
/// the `en` fallback tier in the chain.
#[derive(Clone)]
pub struct SharedLocales {
    en: I18n,
    ex_girl: I18n,
}

impl SharedLocales {
    /// Bind the two already-built machines (the same clones the command
    /// layer serves from, so registrations and reloads stay in sync).
    pub fn new(en: I18n, ex_girl: I18n) -> Self {
        Self { en, ex_girl }
    }

    /// Embedded defaults for both locales, for standalone/test host states.
    ///
    /// Fails only when an embedded locale file is broken (a crate build
    /// defect), the same failure mode as [`I18n::new`].
    pub fn embedded() -> Result<Self, LoadError> {
        Ok(Self {
            en: I18n::new("en")?,
            ex_girl: I18n::new("ex_girl")?,
        })
    }

    /// The machine for `active`. Anything that is not `ex_girl` resolves
    /// through the English machine, the same fallback rule the command
    /// layer applies per invoker.
    pub fn pick(&self, active: &str) -> &I18n {
        match active {
            "ex_girl" => &self.ex_girl,
            _ => &self.en,
        }
    }

    /// Register (or replace) a plugin's locale directory on both machines.
    ///
    /// The directory is the one holding `<active>/<plugin_id>.toml` and
    /// `en/<plugin_id>.toml` — conventionally `locales/` inside the plugin's
    /// own directory. See [`I18n::register_plugin_locale`] for the file
    /// semantics and failure modes; content problems never propagate,
    /// only an unreadable explicit directory does. Both registrations are
    /// attempted; the first error (if any) is returned.
    pub fn register_plugin_locale(&self, plugin_id: &str, dir: &Path) -> Result<(), LoadError> {
        let en = self.en.register_plugin_locale(plugin_id, dir);
        let ex_girl = self.ex_girl.register_plugin_locale(plugin_id, dir);
        en.and(ex_girl)
    }
}

impl std::fmt::Debug for SharedLocales {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedLocales").finish_non_exhaustive()
    }
}
