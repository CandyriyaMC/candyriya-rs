//! The proxy version handshake types.
//!
//! The host advertises its plugin ABI surface through [`ProxyVersion`] so
//! plugins (and the loader) can decide at startup whether they were built
//! against a compatible host. The check is deliberately conservative: a
//! plugin is accepted only when it targets the same major API version and
//! an equal-or-older minor version.

use serde::{Deserialize, Serialize};

/// The plugin ABI version this proxy implementation speaks.
///
/// This is the value a plugin should declare in its `plugin_api` field.
/// The format is `major.minor`, without a patch component: the ABI only
/// changes in minor steps while the API is pre-1.0, and every minor bump
/// is meant to stay backward compatible for plugins built against earlier
/// minor releases.
pub const API_VERSION: &str = "0.1";

/// The implementation identifier a host reports next to the API version.
///
/// Plugins should not branch on this value beyond diagnostics and logging;
/// the API contract is what matters, not who serves it.
pub const IMPLEMENTATION: &str = "candyriya";

/// Identity of the proxy acting as the plugin host.
///
/// Exchanged through the `proxy.version` host function and recorded by the
/// loader for diagnostics. Plain data, safe to move across the plugin ABI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyVersion {
    /// The plugin ABI version the host implements, e.g. `"0.1"`.
    pub api_version: String,
    /// The host implementation identifier, e.g. `"candyriya"`.
    pub implementation: String,
}

impl ProxyVersion {
    /// The version of this build: [`API_VERSION`] + [`IMPLEMENTATION`].
    pub fn current() -> Self {
        Self {
            api_version: API_VERSION.to_owned(),
            implementation: IMPLEMENTATION.to_owned(),
        }
    }

    /// Returns `true` when `plugin_api` can run against this host.
    ///
    /// `plugin_api` must be a `major.minor` pair (see [`API_VERSION`]).
    /// Compatibility requires the same major version and an equal-or-older
    /// minor version: a plugin built against `"0.1"` runs on a host speaking
    /// `"0.2"`, but a plugin built against `"0.2"` is refused by a host
    /// speaking `"0.1"`. Malformed input is simply `false` — comparison is
    /// not an error path.
    pub fn is_api_compatible(&self, plugin_api: &str) -> bool {
        let (host_major, host_minor) = match parse_major_minor(&self.api_version) {
            Some(v) => v,
            None => return false,
        };
        let (plugin_major, plugin_minor) = match parse_major_minor(plugin_api) {
            Some(v) => v,
            None => return false,
        };
        plugin_major == host_major && plugin_minor <= host_minor
    }
}

/// Parses an `major.minor` pair; anything else is rejected.
pub(crate) fn parse_major_minor(input: &str) -> Option<(u64, u64)> {
    let mut parts = input.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    // reject "1.2.3" and friends — the ABI spec is exactly two components
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_matches_consts() {
        let v = ProxyVersion::current();
        assert_eq!(v.api_version, "0.1");
        assert_eq!(v.implementation, "candyriya");
    }

    #[test]
    fn compatibility_matrix() {
        let host = ProxyVersion::current();
        assert!(host.is_api_compatible("0.1"));
        assert!(!host.is_api_compatible("0.2"));
        assert!(!host.is_api_compatible("1.0"));
        assert!(!host.is_api_compatible("0"));
        assert!(!host.is_api_compatible("0.1.0"));
        assert!(!host.is_api_compatible(""));
        assert!(!host.is_api_compatible("zero.one"));
    }

    #[test]
    fn parse_major_minor_accepts_boundaries() {
        assert_eq!(parse_major_minor("0.0"), Some((0, 0)));
        assert_eq!(parse_major_minor("12.34"), Some((12, 34)));
        assert_eq!(parse_major_minor("1.2.3"), None);
        assert_eq!(parse_major_minor("1"), None);
        assert_eq!(parse_major_minor(".1"), None);
        assert_eq!(parse_major_minor("1."), None);
        assert_eq!(parse_major_minor("-1.2"), None);
    }
}
