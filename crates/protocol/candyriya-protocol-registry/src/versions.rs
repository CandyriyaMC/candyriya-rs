//! Parsing and validation of the protocol-versions table.
//!
//! The embedded snapshot is a verbatim copy of PrismarineJS's
//! `data/pc/common/protocolVersions.json`. It contains every release,
//! pre-release and snapshot Minecraft (and the beta/alpha era); the loader
//! keeps only stable releases from 1.7.2 (protocol 4) to the newest 26.x,
//! i.e. entries whose name is a plain dotted-numeric version `M.m[.p]` with
//! `M > 1 || (M == 1 && m >= 7)`.

use serde::Deserialize;

use crate::version::VersionInfo;

/// The raw shapes inside the file we care about. Unknown fields are
/// tolerated so newer snapshots keep parsing with an older binary.
#[derive(Debug, Deserialize)]
struct VersionEntryJson {
    #[serde(rename = "minecraftVersion")]
    minecraft_version: String,
    version: i64,
}

/// One validated, sortable version entry.
#[derive(Debug, Clone)]
pub(crate) struct VersionEntry {
    pub protocol: i32,
    pub name: String,
}

/// Outcome of successfully validating a versions document.
#[derive(Debug)]
pub(crate) struct VersionsTable {
    /// Entries sorted by `(protocol, numeric version)`; non-decreasing protocol ids.
    pub entries: Vec<VersionEntry>,
}

/// Parse and validate a `protocolVersions.json`-shaped document.
///
/// Rules (all hard requirements; the caller decides whether to fall back or
/// propagate):
///
/// - JSON parses as an array; every element has a non-empty
///   `minecraftVersion` (at most 64 chars) and an integer `version` in
///   `0..=2_147_483_647`.
/// - `minecraftVersion` values are unique across the whole document.
/// - At least 83 stable releases from 1.7.2 on must be present, including
///   1.7.2 itself with protocol 4, and at least one entry with protocol
///   777 or newer (the 26.x line).
pub(crate) fn validate(bytes: &[u8]) -> Result<VersionsTable, String> {
    let raw: Vec<VersionEntryJson> =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid JSON or schema: {e}"))?;
    if raw.is_empty() {
        return Err("versions file is an empty array".to_string());
    }

    // uniqueness of names, before any filtering
    let mut seen = std::collections::HashSet::with_capacity(raw.len());
    for entry in &raw {
        if entry.minecraft_version.is_empty() || entry.minecraft_version.len() > 64 {
            return Err(format!(
                "invalid minecraftVersion {:?}: must be 1..=64 chars",
                entry.minecraft_version
            ));
        }
        if !seen.insert(&entry.minecraft_version) {
            return Err(format!(
                "duplicate minecraftVersion {:?}",
                entry.minecraft_version
            ));
        }
        if entry.version < 0 || entry.version > i64::from(i32::MAX) {
            return Err(format!(
                "minecraftVersion {:?}: version {} is outside 0..=2147483647",
                entry.minecraft_version, entry.version
            ));
        }
    }

    // keep stable releases from 1.7.2 onwards
    let mut kept: Vec<VersionEntry> = raw
        .iter()
        .filter_map(|e| release_key(&e.minecraft_version).map(|key| (key, e)))
        .filter(|(key, _)| key.0 > 1 || (key.0 == 1 && key.1 >= 7))
        .map(|(_, e)| VersionEntry {
            protocol: e.version as i32,
            name: e.minecraft_version.clone(),
        })
        .collect();

    if kept.len() < 83 {
        return Err(format!(
            "expected at least 83 stable releases from 1.7.2 onwards, found {}",
            kept.len()
        ));
    }
    if !kept.iter().any(|e| e.name == "1.7.2" && e.protocol == 4) {
        return Err("missing anchor entry 1.7.2 with protocol 4".to_string());
    }
    if !kept.iter().any(|e| e.protocol >= 777) {
        return Err("missing an entry for protocol 777 or newer (the 26.x line)".to_string());
    }

    kept.sort_by(|a, b| {
        a.protocol.cmp(&b.protocol).then_with(|| {
            release_key(&a.name)
                .expect("kept entries are release names")
                .cmp(&release_key(&b.name).expect("kept entries are release names"))
        })
    });

    // monotonicity of ids inside the kept set (construction guarantees it;
    // cheap belt-and-braces for corrupt data and a reminder in review)
    for pair in kept.windows(2) {
        debug_assert!(pair[0].protocol <= pair[1].protocol);
    }

    Ok(VersionsTable { entries: kept })
}

/// `(major, minor, patch)` for a `M.m[.p]` dotted-numeric name, else `None`.
fn release_key(name: &str) -> Option<(u32, u32, u32)> {
    let mut parts = name.split('.');
    let major = parse_component(parts.next()?)?;
    let minor = parse_component(parts.next()?)?;
    let patch = match parts.next() {
        Some(p) => parse_component(p)?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn parse_component(part: &str) -> Option<u32> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

/// The leaked, immutable table the runtime queries. Leaking at startup is
/// the point: lookups are free and the table genuinely lives for the whole
/// process.
pub(crate) fn build_table(entries: Vec<VersionEntry>) -> &'static [VersionInfo] {
    let leaked: Vec<VersionInfo> = entries
        .into_iter()
        .map(|e| {
            let name = Box::leak(e.name.into_boxed_str());
            VersionInfo {
                protocol: e.protocol,
                name,
                has_configuration_state: e.protocol >= 764,
                has_transfer_intent: e.protocol >= 766,
            }
        })
        .collect();
    Box::leak(leaked.into_boxed_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMBEDDED: &[u8] = include_bytes!("../protocol_versions.json");

    #[test]
    fn embedded_snapshot_validates() {
        let table = validate(EMBEDDED).expect("embedded snapshot must validate");
        assert_eq!(table.entries.len(), 83);
        assert!(
            table
                .entries
                .windows(2)
                .all(|w| w[0].protocol <= w[1].protocol)
        );
        assert!(
            table
                .entries
                .iter()
                .any(|e| e.name == "1.7.2" && e.protocol == 4)
        );
        assert!(
            table
                .entries
                .iter()
                .any(|e| e.name == "26.3" && e.protocol == 777)
        );
    }

    #[test]
    fn rejects_garbage_json() {
        assert!(validate(b"not json").is_err());
        assert!(validate(b"{}").is_err());
        assert!(validate(b"[]").is_err());
    }

    #[test]
    fn rejects_duplicate_names() {
        let raw = br#"[
            {"minecraftVersion":"1.7.2","version":4},
            {"minecraftVersion":"1.7.2","version":5}
        ]"#;
        assert!(validate(raw).is_err());
    }

    #[test]
    fn rejects_incomplete_snapshot() {
        // a well-formed array without the 1.7.2 anchor or enough releases
        // must not pass
        let raw = br#"[{"minecraftVersion":"1.8","version":47}]"#;
        assert!(validate(raw).is_err());
    }
}
