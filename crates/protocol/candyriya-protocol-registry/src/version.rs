//! The leaked, runtime-facing struct every lookup goes through.

/// One supported Java Edition release.
///
/// The table is sorted by `(protocol, numeric version)`, so several releases
/// may share one `protocol` (e.g. 1.20 and 1.20.1 both sit on 763). Ids are
/// looked up through [`crate::Registry::from_protocol`], which returns the
/// newest release in its protocol group.
///
/// `has_configuration_state` and `has_transfer_intent` describe what the
/// proxy can rely on for this version; they match the data, not the era
/// name: from 1.20.2 (protocol 764) both peers enter the configuration
/// state after a successful login, and the transfer feature (including the
/// `transfer` login plugin message and transfer packets) arrived in 1.20.5
/// (protocol 766).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VersionInfo {
    /// The protocol id used on the wire for this release.
    pub protocol: i32,
    /// The release name, e.g. `1.12.2` or `26.3`. Borrowed, leaked at load
    /// time, lives for the whole process.
    pub name: &'static str,
    /// Whether this release has the configuration state, i.e. protocol 764
    /// (1.20.2) or newer.
    pub has_configuration_state: bool,
    /// Whether this release supports the transfer feature, i.e. protocol 766
    /// (1.20.5) or newer.
    pub has_transfer_intent: bool,
}

impl std::fmt::Display for VersionInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (protocol {})", self.name, self.protocol)
    }
}
