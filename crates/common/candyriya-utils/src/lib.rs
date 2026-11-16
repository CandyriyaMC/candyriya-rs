//! Shared small primitives used across candyriya crates.
//!
//! This crate is the stable common base of the workspace: small, dependency-light
//! helpers (UTF-8-safe truncation, hex, string newtypes, atomic file writes,
//! random tokens and human readable formatting) that would otherwise be copied
//! into every crate that needs them. Everything here is synchronous,
//! deterministic and free of I/O except [`atomic_write`].
//!
//! No network, no unsafe, no global state.

#![forbid(unsafe_code)]

mod atomic_write;
mod hex;
mod human;
mod non_empty;
mod random;
mod truncate;

pub use atomic_write::atomic_write;
pub use hex::{HexDecodeError, hex_decode, hex_lower};
pub use human::{PrettyDuration, format_uptime, human_bytes};
pub use non_empty::NonEmptyString;
pub use random::{random_token, random_token_alphanumeric};
pub use truncate::{clamp_str, truncate_to_char_boundary};
