//! The data-driven registry: embedded snapshot, file override, auto-refresh.
//!
//! See the crate-level docs for the layer model overview.

use std::collections::HashMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use thiserror::Error;

use crate::ids::{self, ProtocolIds};
use crate::kind::PacketKind;
use crate::state::{ConnectionState, Direction, PacketSlot};
use crate::version::VersionInfo;
use crate::versions::{self, VersionsTable};

const EMBEDDED_VERSIONS: &[u8] = include_bytes!("../protocol_versions.json");
const EMBEDDED_IDS: &[u8] = include_bytes!("../protocol_ids.json");

/// Refreshes fetching more than this many bytes are rejected; the real files
/// are around 37 KiB and 127 KiB at the time of writing.
const MAX_REFRESH_BYTES: usize = 4 * 1024 * 1024;

/// minreq takes whole seconds; sub-second timeouts are rounded up to one.
#[cfg(feature = "refresh")]
const MIN_REFRESH_TIMEOUT_SECS: u64 = 1;

const VERSIONS_FILE: &str = "protocols.json";
const IDS_FILE: &str = "protocol_ids.json";

/// Signature of the fetch closure used by the refresh path; tests inject fakes.
type FetchFn = dyn Fn(&str, Duration) -> Result<Vec<u8>, String>;

/// Where each table was loaded from. Inspect with [`Registry::origin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryOrigin {
    /// The versions table: the embedded snapshot, an override file, or a
    /// refresh applied this run.
    pub versions: TableOrigin,
    /// The packet-id tables. Refresh only ever replaces the versions table;
    /// packet ids come from the embedded snapshot or the override file.
    pub packet_ids: TableOrigin,
}

impl RegistryOrigin {
    fn all_embedded() -> Self {
        Self {
            versions: TableOrigin::Embedded,
            packet_ids: TableOrigin::Embedded,
        }
    }
}

/// Where one table was loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableOrigin {
    /// The snapshot compiled into the binary.
    Embedded,
    /// A file read from the override directory (path shown).
    OverrideFile(PathBuf),
    /// Fetched from the refresh URL this run (URL shown).
    Refreshed(String),
}

/// Non-fatal problems found while loading. Collect them at startup and log;
/// a proxy that only reads these should keep running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryWarning {
    /// An override file was present but could not be read or failed
    /// validation; the embedded snapshot is used for that file.
    OverrideFallback {
        /// The file involved: `protocols.json` or `protocol_ids.json`.
        file: &'static str,
        /// Why the override was rejected.
        reason: String,
    },
    /// A refresh was attempted but failed for any reason (network, HTTP
    /// status, oversized body, invalid JSON, validation, missing packet-id
    /// coverage). The embedded snapshot or the override directory was used
    /// instead; startup continues.
    RefreshFailed {
        /// The URL that was fetched.
        url: String,
        /// Why the refresh was rejected.
        reason: String,
    },
    /// A refresh passed validation and was persisted to the override
    /// directory atomically.
    RefreshApplied {
        /// The URL that was fetched.
        url: String,
    },
    /// A refresh passed validation but could not be written to the override
    /// directory (permissions, full disk, ...). The fetched table is used
    /// in-memory for this run; nothing was persisted.
    RefreshNotPersisted {
        /// The URL that was fetched.
        url: String,
        /// Why it could not be written.
        reason: String,
    },
    /// A supported release in the active versions table has no packet-id
    /// entry; classifying any of its packets returns
    /// [`PacketKind::Unknown`]. The proxy can still forward raw frames.
    PacketIdsMissing {
        /// The protocol id with no packet-id table.
        protocol: i32,
        /// A release name for that protocol, for the log line.
        name: String,
    },
    /// `protocol_ids.json` mentions releases this versions table does not
    /// know (informational; the other direction - a supported release with
    /// no packet ids - is reported via [`RegistryWarning::PacketIdsMissing`]).
    PacketIdsExtraNames {
        /// The names found in the packet-id tables but not in the versions
        /// table.
        names: Vec<String>,
    },
}

impl fmt::Display for RegistryWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OverrideFallback { file, reason } => {
                write!(
                    f,
                    "override file {file} rejected, using embedded snapshot: {reason}"
                )
            }
            Self::RefreshFailed { url, reason } => {
                write!(
                    f,
                    "refresh of {url} failed, keeping current tables: {reason}"
                )
            }
            Self::RefreshApplied { url } => write!(f, "refreshed protocol tables from {url}"),
            Self::RefreshNotPersisted { url, reason } => write!(
                f,
                "refreshed protocol tables from {url} but could not persist them: {reason}"
            ),
            Self::PacketIdsMissing { protocol, name } => write!(
                f,
                "no packet-id table covers protocol {protocol} ({name}); classification returns unknown"
            ),
            Self::PacketIdsExtraNames { names } => write!(
                f,
                "packet-id tables reference releases not in the versions table: {}",
                names.join(", ")
            ),
        }
    }
}

/// Fatal load errors. These can only occur when even the embedded snapshot
/// fails validation - i.e. a broken build - never because of the network or
/// the override directory.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// The embedded snapshot failed validation. This is a build bug; please
    /// report it.
    #[error("embedded protocol snapshot failed validation: {reason}")]
    EmbeddedSnapshotInvalid {
        /// Why the snapshot was rejected.
        reason: String,
    },
}

/// The result of loading a registry: the registry itself plus any warnings
/// collected along the way. Never fails at startup because of the network;
/// fails only in the impossible case of a corrupt embedded snapshot.
pub struct LoadOutcome {
    /// The registry built from the active tables.
    pub registry: Registry,
    /// Warnings collected during loading, in order.
    pub warnings: Vec<RegistryWarning>,
}

/// The Minecraft Java Edition protocol version registry.
///
/// Cheap to hold; the tables are shared, leaked and immutable. Cloning a
/// registry is not supported (use references).
pub struct Registry {
    table: Table,
    ids: ProtocolIds,
    origin: RegistryOrigin,
}

struct Table {
    entries: &'static [VersionInfo],
    by_name: HashMap<&'static str, usize>,
}

impl Registry {
    /// Load only the embedded snapshot. Pure and always works unless the
    /// binary itself was built from a corrupt tree (see
    /// [`RegistryError::EmbeddedSnapshotInvalid`]).
    pub fn embedded() -> Result<Registry, RegistryError> {
        let embedded = Embedded::load()?;
        Ok(Registry::from_parts(
            embedded.table,
            embedded.ids,
            RegistryOrigin::all_embedded(),
        ))
    }

    /// Load with an optional override directory on top of the embedded
    /// snapshot.
    ///
    /// For each of `<override_dir>/protocols.json` (the versions table) and
    /// `<override_dir>/protocol_ids.json` (the packet-id tables): if the file
    /// exists it is read and validated; on any failure the embedded snapshot
    /// is used for that file and a [`RegistryWarning::OverrideFallback`]
    /// records why. A missing file is not an error. See the crate docs for
    /// the exact validation rules.
    pub fn load(override_dir: Option<&Path>) -> Result<LoadOutcome, RegistryError> {
        let embedded = Embedded::load()?;
        let mut warnings = Vec::new();

        let (versions, versions_origin) = match override_dir {
            Some(dir) => {
                match read_override(
                    &dir.join(VERSIONS_FILE),
                    versions::validate,
                    VERSIONS_FILE,
                    &mut warnings,
                ) {
                    Some((vtab, path)) => {
                        (Table::from_versions(vtab), TableOrigin::OverrideFile(path))
                    }
                    None => (embedded.table, TableOrigin::Embedded),
                }
            }
            None => (embedded.table, TableOrigin::Embedded),
        };

        let (ids, ids_origin) = match override_dir {
            Some(dir) => {
                match read_override(&dir.join(IDS_FILE), ids::validate, IDS_FILE, &mut warnings) {
                    Some((ids, path)) => (ids, TableOrigin::OverrideFile(path)),
                    None => (embedded.ids, TableOrigin::Embedded),
                }
            }
            None => (embedded.ids, TableOrigin::Embedded),
        };

        let registry = Registry::from_parts(
            versions,
            ids,
            RegistryOrigin {
                versions: versions_origin,
                packet_ids: ids_origin,
            },
        );
        warnings.extend(registry.coverage_warnings());
        Ok(LoadOutcome { registry, warnings })
    }

    /// Load with an optional override directory, optionally refreshing the
    /// versions table from `refresh_url` first.
    ///
    /// When `refresh_url` is set, a single GET is issued with the given
    /// timeout. The fetched document must pass the same validation as a
    /// `protocols.json` override, and every protocol it references must have
    /// packet ids in the table that would be used. On any failure - network,
    /// HTTP status, oversized body, invalid JSON, validation, missing
    /// coverage - nothing is written and [`RegistryWarning::RefreshFailed`]
    /// is reported; the result is exactly what [`Registry::load`] would
    /// return. On success the table is used for this run and, when
    /// `override_dir` is set, atomically persisted to
    /// `<override_dir>/protocols.json` (see
    /// [`RegistryWarning::RefreshApplied`] /
    /// [`RegistryWarning::RefreshNotPersisted`]). Refresh only ever replaces
    /// the versions table; the packet-id tables come from the override
    /// directory or the embedded snapshot.
    ///
    /// The refresh URL should point at a `protocolVersions.json`-shaped
    /// document, e.g.
    /// `https://raw.githubusercontent.com/PrismarineJS/minecraft-data/master/data/pc/common/protocolVersions.json`.
    /// Packet ids ship with releases of this crate; when Minecraft releases
    /// a new version, bump `protocol_ids.json` (or drop a new one into the
    /// data directory) and then refresh works.
    ///
    /// The actual network fetch is available only with the `refresh` cargo
    /// feature (see the crate-level docs); without it every refresh attempt
    /// fails into the fallback path with [`RegistryWarning::RefreshFailed`].
    pub fn load_or_refresh(
        override_dir: Option<&Path>,
        refresh_url: Option<&str>,
        timeout: Duration,
    ) -> Result<LoadOutcome, RegistryError> {
        Self::load_or_refresh_with(override_dir, refresh_url, timeout, &fetch_with_minreq)
    }

    /// `load_or_refresh` with the fetch step injected; tests use this to
    /// exercise the refresh paths without touching the network.
    pub(crate) fn load_or_refresh_with(
        override_dir: Option<&Path>,
        refresh_url: Option<&str>,
        timeout: Duration,
        fetch: &FetchFn,
    ) -> Result<LoadOutcome, RegistryError> {
        let mut warnings = Vec::new();
        if let Some(url) = refresh_url {
            match refresh_assets(url, timeout, override_dir, &mut warnings, fetch) {
                Ok((table, ids, origin)) => {
                    let registry = Registry::from_parts(table, ids, origin);
                    warnings.extend(registry.coverage_warnings());
                    return Ok(LoadOutcome { registry, warnings });
                }
                Err(reason) => warnings.push(RegistryWarning::RefreshFailed {
                    url: url.to_string(),
                    reason,
                }),
            }
        }

        let mut outcome = Self::load(override_dir)?;
        warnings.extend(outcome.warnings);
        outcome.warnings = warnings;
        Ok(outcome)
    }

    /// Where each table was loaded from.
    pub fn origin(&self) -> &RegistryOrigin {
        &self.origin
    }

    /// The table of supported releases, sorted by `(protocol, numeric version)`.
    pub fn versions(&self) -> &'static [VersionInfo] {
        self.table.entries
    }

    /// Iterate the table of supported releases, sorted by `(protocol, numeric version)`.
    pub fn iter(&self) -> std::slice::Iter<'static, VersionInfo> {
        self.table.entries.iter()
    }

    /// Look a release up by protocol id.
    ///
    /// Several releases share one protocol (1.20 and 1.20.1 are both 763);
    /// the newest release in the protocol group (by numeric version order)
    /// is returned, i.e. the last release shipped with that protocol.
    pub fn from_protocol(&self, protocol: i32) -> Option<&'static VersionInfo> {
        let entries = self.table.entries;
        // last index with entries[i].protocol == protocol
        let index = entries.partition_point(|e| e.protocol <= protocol);
        if index == 0 {
            return None;
        }
        let candidate = &entries[index - 1];
        if candidate.protocol == protocol {
            Some(candidate)
        } else {
            None
        }
    }

    /// Look a release up by name, e.g. `from_name("1.12.2")`.
    pub fn from_name(&self, name: &str) -> Option<&'static VersionInfo> {
        self.table
            .by_name
            .get(name)
            .map(|&i| &self.table.entries[i])
    }

    /// The protocol id of a release, e.g. `protocol_id_of("1.12.2") == Some(340)`.
    pub fn protocol_id_of(&self, name: &str) -> Option<i32> {
        self.from_name(name).map(|v| v.protocol)
    }

    /// The newest supported release (greatest protocol id).
    ///
    /// The table is never empty, so this cannot panic.
    pub fn latest(&self) -> &'static VersionInfo {
        self.table
            .entries
            .last()
            .expect("registry table is never empty")
    }

    /// The packet id for a slot in a protocol, `None` for a known gap on
    /// that protocol or for a protocol with no table.
    pub fn packet_id(&self, protocol: i32, slot: PacketSlot) -> Option<i32> {
        self.ids.get(protocol, slot)
    }

    /// Classify a packet in the known states (login and status).
    ///
    /// Everything the proxy does not track - the play state, the
    /// configuration state, handshake packets other than the serverbound
    /// 0x00, and ids with no slot on this version - comes back as
    /// [`PacketKind::Unknown`].
    pub fn classify(
        &self,
        state: ConnectionState,
        direction: Direction,
        version: &VersionInfo,
        packet_id: i32,
    ) -> PacketKind {
        crate::classify::classify(&self.ids, state, direction, version.protocol, packet_id)
    }

    fn from_parts(table: Table, ids: ProtocolIds, origin: RegistryOrigin) -> Self {
        Self { table, ids, origin }
    }

    fn coverage_warnings(&self) -> Vec<RegistryWarning> {
        let mut warnings = Vec::new();
        for v in self.table.entries {
            if !self.ids.covers(v.protocol) {
                warnings.push(RegistryWarning::PacketIdsMissing {
                    protocol: v.protocol,
                    name: v.name.to_string(),
                });
            }
        }
        let known: std::collections::HashSet<&str> =
            self.table.entries.iter().map(|v| v.name).collect();
        let extra = self.ids.extra_names(|n| known.contains(n));
        if !extra.is_empty() {
            warnings.push(RegistryWarning::PacketIdsExtraNames { names: extra });
        }
        warnings
    }
}

impl Table {
    fn from_versions(table: VersionsTable) -> Self {
        let entries = versions::build_table(table.entries);
        let by_name = entries
            .iter()
            .enumerate()
            .map(|(i, v)| (v.name, i))
            .collect();
        Self { entries, by_name }
    }
}

/// Parse both embedded files; a failure here is a build bug.
struct Embedded {
    table: Table,
    ids: ProtocolIds,
}

impl Embedded {
    fn load() -> Result<Self, RegistryError> {
        let vtab = versions::validate(EMBEDDED_VERSIONS)
            .map_err(|reason| RegistryError::EmbeddedSnapshotInvalid { reason })?;
        let ids = ids::validate(EMBEDDED_IDS)
            .map_err(|reason| RegistryError::EmbeddedSnapshotInvalid { reason })?;
        Ok(Self {
            table: Table::from_versions(vtab),
            ids,
        })
    }
}

/// Read and validate an override file, falling back to the embedded snapshot
/// on any failure. A missing file is not an error.
fn read_override<T>(
    path: &Path,
    validate: impl FnOnce(&[u8]) -> Result<T, String>,
    file: &'static str,
    warnings: &mut Vec<RegistryWarning>,
) -> Option<(T, PathBuf)> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warnings.push(RegistryWarning::OverrideFallback {
                file,
                reason: format!("unreadable {}: {e}", path.display()),
            });
            return None;
        }
    };
    match validate(&bytes) {
        Ok(value) => Some((value, path.to_path_buf())),
        Err(reason) => {
            warnings.push(RegistryWarning::OverrideFallback { file, reason });
            None
        }
    }
}

/// Fetch, validate and persist a refreshed versions table. On any error
/// nothing is written and the caller falls back to the override/embedded
/// tables. The `fetch` closure is injectable so tests never touch the
/// network.
fn refresh_assets(
    url: &str,
    timeout: Duration,
    override_dir: Option<&Path>,
    warnings: &mut Vec<RegistryWarning>,
    fetch: &FetchFn,
) -> Result<(Table, ProtocolIds, RegistryOrigin), String> {
    let bytes = fetch(url, timeout)?;
    if bytes.len() > MAX_REFRESH_BYTES {
        return Err(format!(
            "fetched document exceeds the {MAX_REFRESH_BYTES}-byte limit"
        ));
    }
    let vtab = versions::validate(&bytes)?;

    // ids: override if present and valid, else embedded (needed for the
    // coverage check; a refreshed table without matching ids would make the
    // proxy blind to login packets, so it is rejected outright).
    let embedded = Embedded::load().map_err(|e| e.to_string())?;
    let (ids, ids_origin) = match override_dir {
        Some(dir) => match read_override(&dir.join(IDS_FILE), ids::validate, IDS_FILE, warnings) {
            Some((ids, path)) => (ids, TableOrigin::OverrideFile(path)),
            None => (embedded.ids, TableOrigin::Embedded),
        },
        None => (embedded.ids, TableOrigin::Embedded),
    };

    for entry in &vtab.entries {
        if !ids.covers(entry.protocol) {
            return Err(format!(
                "fetched table references protocol {} ({}), but no packet-id table covers it",
                entry.protocol, entry.name
            ));
        }
    }

    let table = Table::from_versions(vtab);
    let known: std::collections::HashSet<&str> = table.entries.iter().map(|v| v.name).collect();
    let extra = ids.extra_names(|n| known.contains(n));
    if !extra.is_empty() {
        warnings.push(RegistryWarning::PacketIdsExtraNames { names: extra });
    }

    if let Some(dir) = override_dir {
        match persist(dir, &bytes) {
            Ok(()) => warnings.push(RegistryWarning::RefreshApplied {
                url: url.to_string(),
            }),
            Err(reason) => warnings.push(RegistryWarning::RefreshNotPersisted {
                url: url.to_string(),
                reason,
            }),
        }
    }
    Ok((
        table,
        ids,
        RegistryOrigin {
            versions: TableOrigin::Refreshed(url.to_string()),
            packet_ids: ids_origin,
        },
    ))
}

/// Atomically write `protocols.json` into `dir`, creating it if needed.
fn persist(dir: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let target = dir.join(VERSIONS_FILE);
    atomic_write(&target, bytes).map_err(|e| format!("cannot write {}: {e}", target.display()))
}

/// Write `bytes` to `path` atomically: temp file in the same directory,
/// fsync, rename. Cleans up the temp file on failure.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(
        ".{}.tmp-{}-{unique}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The default fetcher: hands the request to [`fetch_with_minreq`] when this
/// crate is built with the `refresh` feature (default: off, because native
/// TLS chains do not build on bare windows-gnu boxes). Without the feature
/// every refresh attempt fails into the documented fallback path: embedded
/// tables, a [`RegistryWarning::RefreshFailed`] warning, startup continues.
#[cfg(feature = "refresh")]
fn fetch_with_minreq(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let secs = timeout.as_secs().max(MIN_REFRESH_TIMEOUT_SECS);
    let response = minreq::get(url)
        .with_timeout(secs)
        .send()
        .map_err(|e| format!("fetch {url}: {e}"))?;
    if !(200..300).contains(&response.status_code) {
        return Err(format!("fetch {url}: HTTP status {}", response.status_code));
    }
    let body = response.as_bytes();
    if body.len() > MAX_REFRESH_BYTES {
        return Err(format!(
            "fetch {url}: body exceeds the {MAX_REFRESH_BYTES}-byte limit"
        ));
    }
    Ok(body.to_vec())
}

#[cfg(not(feature = "refresh"))]
fn fetch_with_minreq(_url: &str, _timeout: Duration) -> Result<Vec<u8>, String> {
    Err("this binary was built without the `refresh` feature; \
         rebuild with --features refresh to enable startup auto-refresh"
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temporary directory removed on drop; unique per test.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "candyriya-registry-test-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }

        fn entries(&self) -> Vec<String> {
            std::fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn embedded_versions_json() -> serde_json::Value {
        serde_json::from_slice(EMBEDDED_VERSIONS).unwrap()
    }

    /// The embedded versions document plus one extra release.
    fn versions_with_extra_release() -> Vec<u8> {
        let mut value = embedded_versions_json();
        value.as_array_mut().unwrap().push(serde_json::json!({
            "minecraftVersion": "99.9",
            "version": 9999,
            "dataVersion": 9999,
            "usesNetty": true,
            "majorVersion": "99",
            "releaseType": "release",
        }));
        serde_json::to_vec(&value).unwrap()
    }

    /// The embedded packet-id tables plus an entry covering protocol 9999.
    fn ids_with_extra_entry() -> Vec<u8> {
        let mut value: serde_json::Value = serde_json::from_slice(EMBEDDED_IDS).unwrap();
        value["protocols"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "protocol": 9999,
                "names": ["99.9"],
                "ids": {
                    "status.serverbound.request": 10,
                    "status.serverbound.ping": 11,
                    "status.clientbound.response": 10,
                    "status.clientbound.pong": 11,
                    "login.serverbound.start": 10,
                    "login.serverbound.encryption_response": 11,
                    "login.serverbound.plugin_response": 12,
                    "login.clientbound.disconnect": 10,
                    "login.clientbound.encryption_request": 11,
                    "login.clientbound.login_success": 12,
                    "login.clientbound.set_compression": 13,
                    "login.clientbound.plugin_request": 14
                },
                "source_note": "hand-crafted for the refresh test"
            }));
        serde_json::to_vec(&value).unwrap()
    }

    fn fetch_ok(body: Vec<u8>) -> impl Fn(&str, Duration) -> Result<Vec<u8>, String> {
        move |_, _| Ok(body.clone())
    }

    #[test]
    fn embedded_loads_clean() {
        let registry = Registry::embedded().expect("embedded snapshot is valid");
        assert_eq!(registry.latest().name, "26.3");
        assert_eq!(registry.latest().protocol, 777);
        assert_eq!(registry.versions().len(), 83);
        assert_eq!(registry.origin().versions, TableOrigin::Embedded);
        assert_eq!(registry.origin().packet_ids, TableOrigin::Embedded);
    }

    #[test]
    fn load_with_no_override_dir_has_no_warnings() {
        let outcome = Registry::load(None).expect("embedded loads");
        assert!(
            outcome.warnings.is_empty(),
            "unexpected warnings: {:?}",
            outcome.warnings
        );
    }

    #[test]
    fn load_with_empty_override_dir_has_no_warnings() {
        let dir = TestDir::new();
        let outcome = Registry::load(Some(dir.path())).expect("embedded loads");
        assert!(
            outcome.warnings.is_empty(),
            "unexpected warnings: {:?}",
            outcome.warnings
        );
    }

    #[test]
    fn override_versions_file_wins() {
        let dir = TestDir::new();
        // rename 26.3 -> 26.4 (i.e. the "future" release table): still a
        // valid 83-release table
        let mut value = embedded_versions_json();
        for entry in value.as_array_mut().unwrap() {
            if entry["minecraftVersion"] == "26.3" {
                entry["minecraftVersion"] = serde_json::json!("26.4");
            }
        }
        dir.write("protocols.json", &serde_json::to_vec(&value).unwrap());

        let outcome = Registry::load(Some(dir.path())).expect("override loads");
        assert!(outcome.registry.from_name("26.3").is_none());
        assert!(outcome.registry.from_name("26.4").is_some());
        assert_eq!(
            outcome.registry.origin().versions,
            TableOrigin::OverrideFile(dir.path().join(VERSIONS_FILE))
        );
        assert_eq!(outcome.registry.origin().packet_ids, TableOrigin::Embedded);
        // the embedded packet ids mention 26.3, which the override table no longer knows
        assert!(outcome.warnings.iter().any(|w| matches!(
            w,
            RegistryWarning::PacketIdsExtraNames { names } if names.iter().any(|n| n == "26.3")
        )));
        // classification still works via the embedded packet ids
        let version = outcome.registry.from_name("26.4").unwrap();
        assert_eq!(
            outcome
                .registry
                .classify(ConnectionState::Login, Direction::Clientbound, version, 3),
            PacketKind::SetCompression
        );
    }

    #[test]
    fn invalid_override_versions_falls_back_to_embedded() {
        let dir = TestDir::new();
        dir.write("protocols.json", b"this is not json");
        let outcome = Registry::load(Some(dir.path())).expect("embedded fallback loads");
        assert_eq!(outcome.registry.latest().name, "26.3");
        assert_eq!(outcome.registry.origin().versions, TableOrigin::Embedded);
        assert!(outcome.warnings.iter().any(|w| matches!(
            w,
            RegistryWarning::OverrideFallback {
                file: "protocols.json",
                ..
            }
        )));
    }

    #[test]
    fn invalid_override_ids_falls_back_to_embedded() {
        let dir = TestDir::new();
        dir.write("protocol_ids.json", b"{\"schema\": 99}");
        let outcome = Registry::load(Some(dir.path())).expect("embedded fallback loads");
        assert_eq!(outcome.registry.origin().packet_ids, TableOrigin::Embedded);
        let version = outcome.registry.from_name("1.8").unwrap();
        assert_eq!(
            outcome
                .registry
                .classify(ConnectionState::Login, Direction::Clientbound, version, 3),
            PacketKind::SetCompression
        );
        assert!(outcome.warnings.iter().any(|w| matches!(
            w,
            RegistryWarning::OverrideFallback {
                file: "protocol_ids.json",
                ..
            }
        )));
    }

    #[test]
    fn override_may_extend_table_with_packet_ids_warning() {
        let dir = TestDir::new();
        dir.write("protocols.json", &versions_with_extra_release());
        let outcome = Registry::load(Some(dir.path())).expect("override loads");
        let version = outcome
            .registry
            .from_name("99.9")
            .expect("override table has 99.9");
        assert_eq!(version.protocol, 9999);
        assert!(version.has_configuration_state);
        assert!(version.has_transfer_intent);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| matches!(w, RegistryWarning::PacketIdsMissing { protocol: 9999, .. }))
        );
        // no ids cover 9999, so everything classifies as Unknown
        assert_eq!(
            outcome
                .registry
                .classify(ConnectionState::Login, Direction::Serverbound, version, 0),
            PacketKind::Unknown(0)
        );
    }

    #[test]
    fn refresh_applies_and_persists_atomically() {
        let dir = TestDir::new();
        dir.write("protocol_ids.json", &ids_with_extra_entry());

        let body = versions_with_extra_release();
        let url = "https://example.invalid/protocolVersions.json";
        let outcome = Registry::load_or_refresh_with(
            Some(dir.path()),
            Some(url),
            Duration::from_secs(1),
            &fetch_ok(body),
        )
        .expect("refresh succeeds");

        assert_eq!(outcome.registry.from_name("99.9").unwrap().protocol, 9999);
        assert_eq!(
            outcome.registry.origin().versions,
            TableOrigin::Refreshed(url.to_string())
        );
        // 99.9 classifies through the overridden packet-id tables
        let version = outcome.registry.from_name("99.9").unwrap();
        assert_eq!(
            outcome
                .registry
                .classify(ConnectionState::Login, Direction::Serverbound, version, 10),
            PacketKind::LoginStart
        );
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| matches!(w, RegistryWarning::RefreshApplied { .. }))
        );

        // atomic persistence: protocols.json written, no temp files left behind
        let entries = dir.entries();
        assert!(entries.contains(&VERSIONS_FILE.to_string()));
        assert!(entries.contains(&IDS_FILE.to_string()));
        assert!(
            entries.iter().all(|e| !e.contains(".tmp-")),
            "leftover temp files: {entries:?}"
        );
    }

    #[test]
    fn refresh_without_packet_id_coverage_falls_back() {
        let dir = TestDir::new(); // no protocol_ids.json override: embedded ids lack 9999
        let body = versions_with_extra_release();
        let outcome = Registry::load_or_refresh_with(
            Some(dir.path()),
            Some("https://example.invalid/protocolVersions.json"),
            Duration::from_secs(1),
            &fetch_ok(body),
        )
        .expect("fallback loads embedded tables");
        assert_eq!(outcome.registry.latest().name, "26.3");
        assert_eq!(outcome.registry.origin().versions, TableOrigin::Embedded);
        assert!(outcome.registry.from_name("99.9").is_none());
        assert!(outcome.warnings.iter().any(|w| matches!(
            w,
            RegistryWarning::RefreshFailed { reason, .. } if reason.contains("9999")
        )));
        // nothing was written
        assert!(
            dir.entries().is_empty(),
            "leftover files: {:?}",
            dir.entries()
        );
    }

    #[test]
    fn refresh_with_invalid_json_falls_back_and_writes_nothing() {
        let dir = TestDir::new();
        let outcome = Registry::load_or_refresh_with(
            Some(dir.path()),
            Some("https://example.invalid/protocolVersions.json"),
            Duration::from_secs(1),
            &fetch_ok(b"garbage, definitely not json".to_vec()),
        )
        .expect("fallback loads embedded tables");
        assert_eq!(outcome.registry.latest().name, "26.3");
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| matches!(w, RegistryWarning::RefreshFailed { .. }))
        );
        assert!(dir.entries().is_empty());
    }

    #[test]
    fn refresh_with_oversized_body_falls_back() {
        let dir = TestDir::new();
        let body = vec![b' '; MAX_REFRESH_BYTES + 1];
        let outcome = Registry::load_or_refresh_with(
            Some(dir.path()),
            Some("https://example.invalid/protocolVersions.json"),
            Duration::from_secs(1),
            &fetch_ok(body),
        )
        .expect("fallback loads embedded tables");
        assert!(outcome.warnings.iter().any(|w| matches!(
            w,
            RegistryWarning::RefreshFailed { reason, .. } if reason.contains("limit")
        )));
        assert!(dir.entries().is_empty());
    }

    #[test]
    fn refresh_url_errors_do_not_touch_disk() {
        let dir = TestDir::new();
        // "not-a-url" never reaches the network: the URL fails to parse client-side
        let outcome =
            Registry::load_or_refresh(Some(dir.path()), Some("not-a-url"), Duration::from_secs(1))
                .expect("fallback loads embedded tables");
        assert_eq!(outcome.registry.latest().name, "26.3");
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| matches!(w, RegistryWarning::RefreshFailed { .. }))
        );
        assert!(dir.entries().is_empty());
    }

    #[test]
    fn atomic_write_replaces_and_cleans_up() {
        let dir = TestDir::new();
        let target = dir.path().join(VERSIONS_FILE);
        atomic_write(&target, b"first").unwrap();
        atomic_write(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");
        assert!(dir.entries().iter().all(|e| !e.contains(".tmp-")));
    }
}
