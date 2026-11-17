//! Kick and disconnect reasons.
//!
//! Minecraft sends kick text inside a login-disconnect (or play-disconnect)
//! packet, and both this ABI and the proxy want one canonical set of
//! pre-built reasons instead of ad-hoc strings scattered across call sites.
//!
//! [`KickReason`] is a plain data enum — it crosses the ABI as JSON — and
//! carries everything needed to render itself as a MiniMessage string:
//!
//! - [`KickReason::key`] names the locale key that backs the reason;
//! - [`KickReason::args`] lists the interpolation arguments (already
//!   sanitized against MiniMessage injection);
//! - [`KickReason::raw_minimessage`] is the embedded-en fallback;
//! - [`KickReason::to_kick_text`] renders through [candyriya-i18n] with a
//!   locale you pass in, falling back to the raw string when the locale is
//!   unknown to the i18n machine;
//! - [`KickReason::to_kick_text_with`] does the same with a shared,
//!   already-built [`I18n`] handle — this is the one the proxy should use,
//!   rebuilding the locale tables on every kick is fine but pointless.
//!
//! [`DisconnectReason`] is a type alias, not a separate type: the kick
//! packet and the disconnect packet carry the same kinds of messages, so
//! duplicating the enum would only create two things to keep in sync.
//!
//! ## Locale keys
//!
//! | Variant | i18n key | Arguments |
//! | --- | --- | --- |
//! | `ServerFull` | `candyriya.kick.server_full` | |
//! | `Maintenance` | `candyriya.kick.maintenance` | |
//! | `TransferUnavailable` | `candyriya.kick.transfer_unavailable` | |
//! | `VersionMismatch` | `candyriya.kick.version_mismatch` | |
//! | `NeverAuthenticated` | `candyriya.kick.never_authenticated` | |
//! | `ForcedHost` | `candyriya.kick.forced_host_not_found` | `{host}` |
//! | `Shutdown` | `candyriya.kick.shutdown` | |
//! | `Unreachable` | `candyriya.kick.backend_unreachable` | |
//!
//! `{host}` is a client-controlled value (the address the player tried to
//! join); it is sanitized with [`escape_minimessage`] before interpolation
//! so it can never smuggle tags into the rendered message.
//!
//! ## MiniMessage fallback
//!
//! The `raw_minimessage` strings mirror the embedded English locale. They
//! exist so the enum is useful even where candyriya-i18n cannot run (tests,
//! embedded tooling, unknown locales) — they are not a second source of
//! truth and should never diverge from `embedded/en/candyriya.toml`.

use candyriya_i18n::I18n;
use serde::{Deserialize, Serialize};

use crate::minimessage::escape_minimessage;

/// The reason a player is being kicked (or their connection is being
/// disconnected).
///
/// Serializes as a snake_case string for unit variants and as
/// `{"variant": {"field": ...}}` for struct variants — see the module docs
/// for the exact wire names. All variants are self-contained; nothing
/// references live proxy state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KickReason {
    /// Every server the player could join is full.
    ServerFull,
    /// The proxy is in maintenance mode and refuses new (or all) joins.
    Maintenance,
    /// Transfers between backends are not available on this proxy.
    TransferUnavailable,
    /// The client's protocol version is not supported.
    VersionMismatch,
    /// The session was never authenticated and cannot be trusted.
    NeverAuthenticated,
    /// No forced host matched the address the player tried to join.
    ForcedHost {
        /// The address the player tried to join, client-controlled.
        host: String,
    },
    /// The proxy is shutting down and kicking everyone gracefully.
    Shutdown,
    /// No backend server could be reached.
    Unreachable,
}

/// The reason a player is being disconnected from the proxy.
///
/// A type alias of [`KickReason`], not a separate enum: the kick and
/// disconnect packets carry the same pre-built messages.
pub type DisconnectReason = KickReason;

impl KickReason {
    /// The i18n key that backs this reason, e.g. `"candyriya.kick.server_full"`.
    pub fn key(&self) -> &'static str {
        match self {
            Self::ServerFull => "candyriya.kick.server_full",
            Self::Maintenance => "candyriya.kick.maintenance",
            Self::TransferUnavailable => "candyriya.kick.transfer_unavailable",
            Self::VersionMismatch => "candyriya.kick.version_mismatch",
            Self::NeverAuthenticated => "candyriya.kick.never_authenticated",
            Self::ForcedHost { .. } => "candyriya.kick.forced_host_not_found",
            Self::Shutdown => "candyriya.kick.shutdown",
            Self::Unreachable => "candyriya.kick.backend_unreachable",
        }
    }

    /// The interpolation arguments for the locale string, with every
    /// client-controlled value sanitized against MiniMessage injection.
    ///
    /// The returned key-value pairs match the `{token}` placeholders in the
    /// locale values.
    pub fn args(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::ForcedHost { host } => {
                vec![("host", escape_minimessage(host))]
            }
            _ => Vec::new(),
        }
    }

    /// The raw English MiniMessage fallback, with arguments already
    /// interpolated and sanitized.
    ///
    /// Used when the locale is unknown to the i18n machine, or when a
    /// shared [`I18n`] is not available. Mirrors the embedded English
    /// locale strings.
    pub fn raw_minimessage(&self) -> String {
        match self {
            Self::ServerFull => "<red>The server is full.<reset>".to_owned(),
            Self::Maintenance => "<red>Kicked: the proxy is in maintenance mode.<reset>".to_owned(),
            Self::TransferUnavailable => {
                "<red>Transfers are not available on this proxy.<reset>".to_owned()
            }
            Self::VersionMismatch => "<red>Your client version is not supported.<reset>".to_owned(),
            Self::NeverAuthenticated => {
                "<red>Could not authenticate your connection.<reset>".to_owned()
            }
            Self::ForcedHost { host } => {
                let host = escape_minimessage(host);
                format!("<red>No server found for this address: <gold>{host}<reset><reset>")
            }
            Self::Shutdown => {
                "<red>The proxy is shutting down. Please reconnect later.<reset>".to_owned()
            }
            Self::Unreachable => {
                "<red>Could not reach any server. Try again later.<reset>".to_owned()
            }
        }
    }

    /// Renders the reason into a MiniMessage string through `i18n`.
    ///
    /// This is the hot-path variant: pass the proxy's shared handle and no
    /// new locale tables are built. If the key has vanished from every
    /// locale (e.g. a broken override removed it), the raw English fallback
    /// is produced instead — a player must never see a bare locale key as a
    /// kick message.
    pub fn to_kick_text_with(&self, i18n: &I18n) -> String {
        let args = self.args();
        let refs: Vec<(&str, &str)> = args
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        let rendered = i18n.t(self.key(), &refs);
        if rendered == self.key() {
            self.raw_minimessage()
        } else {
            rendered
        }
    }

    /// Renders the reason into a MiniMessage string for `locale`.
    ///
    /// `locale` must be a locale the embedded i18n machine knows
    /// (`"en"` or `"ex_girl"`); anything else falls back to the embedded
    /// English raw string. Builds a fresh [`I18n`] handle on every call —
    /// fine on the cold kick path, wrong on a hot loop; use
    /// [`KickReason::to_kick_text_with`] with a shared handle there.
    pub fn to_kick_text(&self, locale: &str) -> String {
        match I18n::new(locale) {
            Ok(i18n) => self.to_kick_text_with(&i18n),
            Err(_) => self.raw_minimessage(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_reasons() -> Vec<KickReason> {
        vec![
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
        ]
    }

    #[test]
    fn raw_fallbacks_are_non_empty_minimessage() {
        for reason in all_reasons() {
            let raw = reason.raw_minimessage();
            assert!(!raw.is_empty(), "{reason:?} must render non-empty");
            assert!(
                crate::parse_minimessage(&raw).is_ok(),
                "{reason:?} raw fallback must parse as MiniMessage: {raw:?}"
            );
        }
    }

    #[test]
    fn serde_wire_names() {
        let cases = [
            (KickReason::ServerFull, "\"server_full\""),
            (KickReason::Maintenance, "\"maintenance\""),
            (KickReason::TransferUnavailable, "\"transfer_unavailable\""),
            (KickReason::VersionMismatch, "\"version_mismatch\""),
            (KickReason::NeverAuthenticated, "\"never_authenticated\""),
            (KickReason::Shutdown, "\"shutdown\""),
            (KickReason::Unreachable, "\"unreachable\""),
        ];
        for (reason, json) in cases {
            let serialized = serde_json::to_string(&reason).expect("serialize");
            assert_eq!(serialized, json);
            let back: KickReason = serde_json::from_str(&serialized).expect("deserialize");
            assert_eq!(back, reason);
        }
        let host = KickReason::ForcedHost {
            host: "x.example.com".to_owned(),
        };
        let json = serde_json::to_string(&host).expect("serialize");
        let back: KickReason = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, host);
    }

    #[test]
    fn forced_host_sanitizes_minimessage_injection() {
        let reason = KickReason::ForcedHost {
            host: "<gold>fake</gold>.example.com".to_owned(),
        };
        for rendered in [reason.raw_minimessage(), reason.to_kick_text("en")] {
            assert!(
                !rendered.contains("<gold>fake"),
                "host tags must not survive: {rendered:?}"
            );
            assert!(
                rendered.contains("\u{2039}gold\u{203a}fake"),
                "host must be escaped: {rendered:?}"
            );
        }
    }
}
