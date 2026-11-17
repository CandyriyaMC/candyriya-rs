//! Locale loader for Candyriya and its plugins.
//!
//! User-facing strings for the proxy live in two embedded locales:
//!
//! * `en` — default English;
//! * `ex_girl` — Russian, written in one very opinionated voice.
//!
//! Both are TOML files with flat quoted keys (`"proxy.backend.unreachable" = "…"`),
//! where the values are [MiniMessage](https://docs.adventure.kyori.net/minimessage/format.html)
//! strings. No nesting, no tables of sections, one flat `map[string]string`
//! per file.
//!
//! # On-disk layout and hot reload
//!
//! Embedded locales are the source of truth for defaults. A data directory
//! can override them per file:
//!
//! ```text
//! <data_dir>/locales/en/common.toml        (mandatory file name)
//! <data_dir>/locales/en/candyriya.toml     (core proxy strings)
//! <data_dir>/locales/ex_girl/common.toml
//! <data_dir>/locales/ex_girl/candyriya.toml
//! ```
//!
//! Override files live next to the binary's data directory, never inside this
//! crate, and the repository root does not get a `locales/` directory. Files
//! that are missing simply do not override; files that fail to parse are a
//! `tracing::warn!` and the embedded defaults are used for *that file only*.
//! [`I18n::reload`] re-reads the override directory atomically via `arc-swap`,
//! so `/candyriya reload` picks up changes without a restart.
//!
//! # Fallback chain
//!
//! Looking up a key in locale `L` (active locale) checks, in order:
//!
//! 1. `L/candyriya.toml` (core), then each registered plugin file, in
//!    registration order;
//! 2. `en/candyriya.toml`, then each registered plugin file;
//! 3. `L/common.toml`;
//! 4. `en/common.toml`;
//! 5. the key itself, with a one-time `tracing::warn!` per missing key.
//!
//! This function never panics and a missing key is never an error.
//!
//! # MiniMessage note
//!
//! The values are MiniMessage strings; the proxy parses them with
//! `lobsterchat` at the point of use. This crate only stores and interpolates
//! them, it deliberately does not render or validate them at lookup time.
//!
//! # Safety and scope
//!
//! This crate performs no network requests and writes no temporary files. It
//! only reads the override directory you pass in.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod error;
mod locale;

pub use error::LoadError;
pub use locale::I18n;
