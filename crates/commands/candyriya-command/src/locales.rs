//! Per-invoker locale handles for command output.
//!
//! Command output is rendered per invocation: a console banner and a
//! player's chat never leak each other's language. [`Locales`] binds the two
//! locales the proxy ships, one handle each, and hands out the right one
//! for a given [`CommandSource`].

use candyriya_i18n::{I18n, LoadError};

use crate::source::CommandSource;

/// The two locales the proxy ships, bound to one handle each.
///
/// Command output picks the invoker's locale (`ex_girl` when that is what
/// the source carries, English otherwise), so the console banner and a
/// player's chat never leak each other's language.
#[derive(Clone)]
pub struct Locales {
    en: I18n,
    ex_girl: I18n,
}

impl Locales {
    /// Binds two ready-made locale handles (embedded defaults or overrides —
    /// whatever the composition root built).
    pub fn new(en: I18n, ex_girl: I18n) -> Self {
        Self { en, ex_girl }
    }

    /// Embedded defaults for both locales. Fails only when an embedded
    /// locale file is broken (a crate build defect), the same failure mode
    /// as [`I18n::new`].
    ///
    /// # Errors
    ///
    /// See [`I18n::new`].
    pub fn embedded() -> Result<Self, LoadError> {
        Ok(Self {
            en: I18n::new("en")?,
            ex_girl: I18n::new("ex_girl")?,
        })
    }

    pub(crate) fn pick(&self, source: &CommandSource) -> &I18n {
        match source.locale() {
            Some("ex_girl") => &self.ex_girl,
            _ => &self.en,
        }
    }
}
