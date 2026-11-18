//! [`PluginMetadata`]: who the plugin is, as reported at init time.

use crate::buf::CandyriyaBuf;

/// The identity a plugin reports when it initialises.
///
/// The plugin fills this itself and returns it inside
/// [`crate::PluginInitOutput`]; the proxy reads the buffers, copies the
/// bytes it wants to keep, and validates them before treating the plugin as
/// loadable. Buffers point at plugin-owned memory (in practice `&'static`
/// bytes) and must outlive the call; the proxy must not retain the raw
/// pointers.
///
/// `description` is optional: an empty buffer means "not provided". The
/// `id`, `version` and `name` buffers must be non-empty and valid when the
/// reported [`crate::CandyriyaStatus::Ok`] is `Ok` — the proxy refuses to
/// load a plugin that reports success with an empty identity.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginMetadata {
    /// Plugin identifier, e.g. `greeting`. Same charset rules as the
    /// `plugin.toml` `id`: `[a-z0-9_.-]{1,64}`, no leading/trailing dots,
    /// no `..`. Validated by the proxy, not by this crate.
    pub id: CandyriyaBuf,
    /// The plugin's own semver version, e.g. `0.1.0`.
    pub version: CandyriyaBuf,
    /// Human-readable display name, e.g. `Greeting`.
    pub name: CandyriyaBuf,
    /// One-line description; empty means absent.
    pub description: CandyriyaBuf,
}

impl PluginMetadata {
    /// The zeroed metadata: every buffer empty. Used for failure outputs —
    /// a plugin that cannot initialise still returns a well-formed output,
    /// just an empty one.
    pub const EMPTY: Self = Self {
        id: CandyriyaBuf::EMPTY,
        version: CandyriyaBuf::EMPTY,
        name: CandyriyaBuf::EMPTY,
        description: CandyriyaBuf::EMPTY,
    };

    /// Builds metadata from raw byte slices — typically `&'static` ones.
    ///
    /// # Panics
    ///
    /// Panics (via [`CandyriyaBuf::new`]) if a slice is longer than
    /// `u32::MAX` bytes — a programming error, never a runtime condition.
    #[must_use]
    pub fn new(id: &[u8], version: &[u8], name: &[u8], description: &[u8]) -> Self {
        Self {
            id: CandyriyaBuf::new(id),
            version: CandyriyaBuf::new(version),
            name: CandyriyaBuf::new(name),
            description: CandyriyaBuf::new(description),
        }
    }

    /// `true` when every mandatory buffer (`id`, `version`, `name`) is
    /// non-empty. The proxy additionally checks the byte contents against
    /// the manifest rules; this is only the "not empty" floor.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.id.is_empty() && !self.version.is_empty() && !self.name.is_empty()
    }
}
