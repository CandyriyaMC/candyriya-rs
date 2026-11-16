//! Error types for the i18n loader.
//!
//! Everything here is an I/O or configuration problem, never a programming
//! error, so the crate never panics. Display messages are English on purpose:
//! they are meant for operators reading logs, not for players.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Ways loading or reloading locale data can fail.
///
/// Content problems are *not* fatal: an unreadable or malformed override file
/// is logged with `tracing::warn!` and the embedded defaults are used for the
/// affected file only. The variants below are reserved for "the operator
/// asked for something we cannot do" situations.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoadError {
    /// The requested locale is not one of the two built-in locales.
    UnknownLocale {
        /// The locale identifier that was rejected.
        locale: String,
    },
    /// An override directory or file that the operator explicitly asked for
    /// cannot be read as a directory.
    Io {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// A plugin id that was passed to [`crate::I18n::register_plugin_locale`]
    /// is not safe to use as a file name.
    InvalidPluginId {
        /// The plugin id that was rejected.
        id: String,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownLocale { locale } => write!(
                f,
                "unknown locale {locale:?}: expected one of \"en\", \"ex_girl\""
            ),
            Self::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::InvalidPluginId { id } => write!(
                f,
                "invalid plugin id {id:?}: expected 1 to 64 characters of [a-z0-9_.-], starting with a letter or digit"
            ),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
