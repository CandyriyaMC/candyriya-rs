//! The parsed `plugin.toml` model.
//!
//! Every plugin ships a `plugin.toml` next to its WASM module. The loader
//! reads it, hands it to [`PluginMetadata::validate`] and, only on success,
//! uses it to wire the plugin into the proxy (dependencies, cache quotas,
//! host-function whitelist, exports exposed to other plugins).
//!
//! This module owns the *shape* of the manifest and cheap structural
//! validation. Anything that needs the filesystem, network or another
//! plugin's metadata belongs to the loader, not here.
//!
//! Note on field naming: the plugin API version is spelled `plugin_api`
//! (not `api`) because `api` is taken by the `[api]` table carrying the
//! exports whitelist — TOML does not allow the same key as both a string
//! and a table.

use serde::{Deserialize, Serialize};

use crate::version::parse_major_minor;

/// Errors produced while validating [`PluginMetadata`] or while checking
/// MiniMessage input in [`crate::parse_minimessage`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MetadataError {
    /// One or more validation rules failed. The message contains every
    /// issue found, separated by `"; "`, so a broken manifest can be fixed
    /// in a single pass instead of a edit-load-fix loop.
    #[error("invalid plugin metadata: {0}")]
    Validation(String),
    /// The input was not a MiniMessage string this ABI can fully parse.
    /// lobsterchat itself never reports parse failures (it silently stops
    /// at the first bad tag); `parse_minimessage` rejects those inputs
    /// instead, so callers can trust an `Ok` means nothing was dropped.
    #[error("invalid MiniMessage input: {reason}")]
    MiniMessage {
        /// What was wrong and where, e.g. `unknown tag <rainbow> at byte 3`.
        reason: String,
    },
}

/// A single entry of the `[api].exports` table.
///
/// Deliberately just a name. The runtime ABI of a plugin export is fixed
/// by extism (arguments and return are bytes); describing richer types
/// here would duplicate the host function signatures without adding
/// safety.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiExport {
    /// The WASM export name other plugins may call through `call_api`.
    pub name: String,
}

/// The `[api]` section of `plugin.toml`.
///
/// Declares which exports of this plugin other plugins are allowed to
/// invoke through the host `call_api(plugin_id, fn_name, args)` function.
/// An empty list — or a missing `[api]` section — means the plugin exports
/// nothing to its peers; only explicitly listed names are callable.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ApiInfo {
    /// Exports other plugins may call. Empty means none.
    #[serde(default)]
    pub exports: Vec<ApiExport>,
}

/// The parsed contents of a `plugin.toml` manifest.
///
/// Unknown keys are ignored on purpose: forward compatibility with newer
/// manifest fields beats strictness for a file the host and plugin authors
/// do not upgrade in lockstep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginMetadata {
    /// Plugin identifier. `[a-z0-9_.-]{1,64}`, no `..`, no leading or
    /// trailing `.` — it is used as a directory name and in log lines.
    pub id: String,
    /// Semver version of the plugin itself, e.g. `"1.4.0"`.
    pub version: String,
    /// Human-readable display name.
    pub name: String,
    /// What the plugin does; shown in logs and tooling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Plugin authors, e.g. `["a@example.com"]` or `["Alice <a@example.com>"]`.
    #[serde(default)]
    pub authors: Vec<String>,
    /// Plugin website or repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    /// License identifier or text, e.g. `"GPL-3.0-only"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Name of the WASM export the host calls when the plugin loads.
    pub plugin_main: String,
    /// The candyriya plugin ABI version the plugin was built against,
    /// e.g. `"0.1"`. See [`crate::ProxyVersion`] for the compatibility rule.
    pub plugin_api: String,
    /// Hard dependencies: plugin/library ids that must load first.
    #[serde(default)]
    pub depend: Vec<String>,
    /// Soft dependencies: loaded first when present, ignored otherwise.
    #[serde(default)]
    pub softdepend: Vec<String>,
    /// Whitelisted host capabilities, as opaque strings
    /// (e.g. `["players.read", "servers.switch"]`). Interpreted by the
    /// loader; this crate only checks they are non-empty and unique.
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Maximum bytes the plugin may use in its cache namespace.
    /// `None` means the proxy default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_quota_bytes: Option<u64>,
    /// Maximum number of cache resources the plugin may hold.
    /// `None` means the proxy default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_quota_resources: Option<u64>,
    /// The `[api]` section, listing exports callable by other plugins.
    #[serde(default)]
    pub api: ApiInfo,
}

impl PluginMetadata {
    /// Validates the manifest, collecting every problem found.
    ///
    /// The loader should call this right after deserializing and refuse to
    /// load the plugin on `Err`. All issues are reported in one error so
    /// the author can fix the manifest without a edit-restart loop.
    pub fn validate(&self) -> Result<(), MetadataError> {
        let mut issues = Vec::new();

        if let Err(why) = validate_resource_id(&self.id) {
            issues.push(format!("id {why}"));
        }

        if let Err(why) = semver::Version::parse(&self.version) {
            issues.push(format!("version {:?} is not semver: {why}", self.version));
        }

        if self.name.trim().is_empty() {
            issues.push("name must not be empty".to_owned());
        }

        for (i, author) in self.authors.iter().enumerate() {
            if author.trim().is_empty() {
                issues.push(format!("authors[{i}] must not be empty"));
            }
        }

        if let Some(website) = &self.website
            && !(website.starts_with("http://") || website.starts_with("https://"))
        {
            issues.push(format!(
                "website {website:?} must start with http:// or https://"
            ));
        }

        if self.plugin_main.trim().is_empty() {
            issues.push("plugin_main must not be empty".to_owned());
        } else if !is_valid_export_name(&self.plugin_main) {
            issues.push(format!(
                "plugin_main {:?} is not a valid export name ([A-Za-z_][A-Za-z0-9_.-]{{0,63}})",
                self.plugin_main
            ));
        }

        if parse_major_minor(&self.plugin_api).is_none() {
            issues.push(format!(
                "plugin_api {:?} must be a major.minor pair, e.g. \"0.1\"",
                self.plugin_api
            ));
        }

        for (i, dep) in self.depend.iter().enumerate() {
            if let Err(why) = validate_resource_id(dep) {
                issues.push(format!("depend[{i}] {why}"));
            }
        }
        for (i, dep) in self.softdepend.iter().enumerate() {
            if let Err(why) = validate_resource_id(dep) {
                issues.push(format!("softdepend[{i}] {why}"));
            }
        }
        for dep in &self.depend {
            if self.softdepend.contains(dep) {
                issues.push(format!("{dep:?} is in both depend and softdepend"));
            }
        }

        for (i, permission) in self.permissions.iter().enumerate() {
            if permission.trim().is_empty() {
                issues.push(format!("permissions[{i}] must not be empty"));
            } else if permission.trim() != permission {
                issues.push(format!(
                    "permissions[{i}] {permission:?} has leading or trailing whitespace"
                ));
            }
        }
        if let Some(dupe) = first_duplicate(&self.permissions) {
            issues.push(format!("permissions contains duplicate {dupe:?}"));
        }

        for (i, export) in self.api.exports.iter().enumerate() {
            if !is_valid_export_name(&export.name) {
                issues.push(format!(
                    "api.exports[{i}] {:?} is not a valid export name ([A-Za-z_][A-Za-z0-9_.-]{{0,63}})",
                    export.name
                ));
            }
        }
        if let Some(dupe) = first_duplicate(self.api.exports.iter().map(|e| &e.name)) {
            issues.push(format!("api.exports contains duplicate {dupe:?}"));
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(MetadataError::Validation(issues.join("; ")))
        }
    }
}

/// Plugin/library ids: `[a-z0-9_.-]{1,64}`, no `..` anywhere, never a
/// leading or trailing dot. This is what makes `plugins/<id>/` a safe
/// directory name on every filesystem we care about.
pub(crate) fn validate_resource_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 64 {
        return Err(format!("{id:?} must be 1 to 64 bytes long"));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
    {
        return Err(format!(
            "{id:?} may only contain lowercase ASCII letters, digits, `_`, `.` and `-`"
        ));
    }
    if id.starts_with('.') || id.ends_with('.') {
        return Err(format!("{id:?} must not start or end with `.`"));
    }
    if id.contains("..") {
        return Err(format!("{id:?} must not contain `..`"));
    }
    Ok(())
}

/// Export names (WASM exports, host functions): `[A-Za-z_][A-Za-z0-9_.-]{0,63}`.
pub(crate) fn is_valid_export_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' => {}
        _ => return false,
    }
    name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Returns the first duplicate item, comparing by value.
fn first_duplicate<T>(items: T) -> Option<T::Item>
where
    T: IntoIterator,
    T::Item: Clone + Ord,
{
    let mut seen = std::collections::BTreeSet::new();
    items.into_iter().find(|item| !seen.insert(item.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_id_rules() {
        assert!(validate_resource_id("core").is_ok());
        assert!(validate_resource_id("a_b-c.d.1").is_ok());
        assert!(validate_resource_id("").is_err());
        assert!(validate_resource_id(&"x".repeat(65)).is_err());
        assert!(validate_resource_id("UPPER").is_err());
        assert!(validate_resource_id(".hidden").is_err());
        assert!(validate_resource_id("trailing.").is_err());
        assert!(validate_resource_id("a..b").is_err());
        assert!(validate_resource_id("with space").is_err());
        assert!(validate_resource_id("slash/name").is_err());
    }

    #[test]
    fn export_name_rules() {
        assert!(is_valid_export_name("on_login"));
        assert!(is_valid_export_name("_private"));
        assert!(is_valid_export_name("a"));
        assert!(!is_valid_export_name("0starts_with_digit"));
        assert!(!is_valid_export_name(""));
        assert!(!is_valid_export_name("has space"));
        assert!(!is_valid_export_name("has/slash"));
        assert!(!is_valid_export_name(&"a".repeat(65)));
    }

    #[test]
    fn duplicate_detection() {
        assert_eq!(first_duplicate(vec!["a", "b", "a", "c"]), Some("a"));
        assert_eq!(first_duplicate(vec!["a", "b"]), None::<&str>);
    }
}
