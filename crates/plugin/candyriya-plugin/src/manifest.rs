//! The parsed and validated `plugin.toml` manifest.
//!
//! [`PluginManifest::from_toml`] does deserialization plus *structural*
//! validation (id charset, semver, non-empty fields, ...) through
//! [`candyriya_plugin_api::PluginMetadata`]. Checking a *set* of manifests for
//! duplicate ids is a separate, set-level operation: [`validate_unique`].
//! Keeping the two apart mirrors the split the loader needs — parse each
//! manifest on its own, then check the whole discovered set in one go.

use std::path::{Path, PathBuf};

use candyriya_plugin_api::{MetadataError, PluginMetadata};

/// A `plugin.toml` that parsed and passed structural validation.
///
/// This type deliberately owns the validation result: an instance only ever
/// exists when [`PluginMetadata::validate`] succeeded, so a
/// `PluginManifest` in hand is one the proxy can reason about. Anything the
/// manifest cannot express — whether the wasm module exists, whether
/// `plugin_main` is actually exported, whether the id is unique in the
/// discovered set — is the loader's job.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    metadata: PluginMetadata,
}

impl PluginManifest {
    /// Parse and validate `plugin.toml` from its raw text.
    ///
    /// Fails on TOML syntax errors, on schema mismatches (missing `id`,
    /// `version`, `plugin_main`, `plugin_api`, ...), and on every rule
    /// [`PluginMetadata::validate`] enforces. TOML errors are wrapped in
    /// [`MetadataError::Validation`] so callers have a single error channel.
    pub fn from_toml(content: &str) -> Result<Self, MetadataError> {
        let metadata: PluginMetadata = toml::from_str(content)
            .map_err(|e| MetadataError::Validation(format!("plugin.toml is not valid: {e}")))?;
        metadata.validate()?;
        Ok(Self { metadata })
    }

    /// Borrow the validated metadata.
    pub fn metadata(&self) -> &PluginMetadata {
        &self.metadata
    }

    /// Consume the manifest and return the validated metadata.
    pub fn into_metadata(self) -> PluginMetadata {
        self.metadata
    }

    /// The plugin identifier (`[a-z0-9_.-]{1,64}`).
    pub fn id(&self) -> &str {
        &self.metadata.id
    }

    /// The name of the wasm export the host calls when the plugin loads.
    pub fn plugin_main(&self) -> &str {
        &self.metadata.plugin_main
    }
}

/// One duplicate group found by [`validate_unique`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateId {
    /// The id shared by several candidates.
    pub id: String,
    /// Where each occurrence lives, in the order they were passed in.
    pub origins: Vec<PathBuf>,
}

/// All duplicate id groups found in a plugin set.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("duplicate plugin id(s): {}", .groups.iter().map(|g| format!("`{}` in {} places", g.id, g.origins.len())).collect::<Vec<_>>().join(", "))]
pub struct DuplicatePluginIds {
    /// Every duplicate group, in first-appearance order.
    pub groups: Vec<DuplicateId>,
}

/// Check that every id in `plugin_set` is unique.
///
/// Takes `(id, origin)` pairs in the caller's preferred load order and
/// reports every group of colliding ids — libraries and plugins are checked
/// together, because an id is an id no matter which directory it came from.
/// The *first* origin of a group is the one the loader will keep; the error
/// lists all of them so the caller can report or drop precisely.
///
/// ```
/// use std::path::Path;
/// use candyriya_plugin::validate_unique;
///
/// let ok = validate_unique([
///     ("core", Path::new("libraries/core")),
///     ("web", Path::new("plugins/web")),
/// ]);
/// assert!(ok.is_ok());
///
/// let dup = validate_unique([
///     ("core", Path::new("libraries/core")),
///     ("core", Path::new("plugins/core")),
/// ])
/// .unwrap_err();
/// assert_eq!(dup.groups[0].id, "core");
/// assert_eq!(dup.groups[0].origins.len(), 2);
/// ```
pub fn validate_unique<'a, I>(plugin_set: I) -> Result<(), DuplicatePluginIds>
// the iterator item uses a shared reference; lifetime stays descriptive
where
    I: IntoIterator<Item = (&'a str, &'a Path)>,
{
    let mut origins: std::collections::BTreeMap<&str, Vec<PathBuf>> =
        std::collections::BTreeMap::new();
    let mut order: Vec<&str> = Vec::new();

    for (id, origin) in plugin_set {
        if let Some(list) = origins.get_mut(id) {
            list.push(origin.to_path_buf());
        } else {
            origins.insert(id, vec![origin.to_path_buf()]);
            order.push(id);
        }
    }

    let groups: Vec<DuplicateId> = order
        .into_iter()
        .filter_map(|id| {
            let list = origins.remove(id)?;
            (list.len() > 1).then(|| DuplicateId {
                id: id.to_owned(),
                origins: list,
            })
        })
        .collect();

    if groups.is_empty() {
        Ok(())
    } else {
        Err(DuplicatePluginIds { groups })
    }
}
