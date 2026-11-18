//! Implementation of [`JsonPlayerStore`], the file-backed
//! [`PlayerStore`][candyriya_player::PlayerStore].
//!
//! See the crate-level docs for the file format and the atomic-write policy.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use candyriya_player::{GameProfile, OfflinePlayer, PlayerStore, PlayerStoreError};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::error::StoreError;

/// Hard cap on the size of the backing file, in bytes (100 MiB).
///
/// A valid store file is a JSON array of records; a file bigger than
/// this is treated as corrupt rather than opened. The cap is checked
/// before any bytes are read so a hostile or accidentally huge file is
/// refused up front.
pub const MAX_FILE_SIZE_BYTES: u64 = 100 * 1024 * 1024;

/// Process-wide counter used to make temporary file names unique between
/// concurrent saves (and between store instances) in this process.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Refuse a file whose size exceeds [`MAX_FILE_SIZE_BYTES`].
fn ensure_size_within_cap(path: &Path, size: u64) -> Result<(), StoreError> {
    if size > MAX_FILE_SIZE_BYTES {
        return Err(StoreError::TooLarge {
            path: path.to_path_buf(),
            size,
            max: MAX_FILE_SIZE_BYTES,
        });
    }
    Ok(())
}

/// Parse and validate the raw bytes of a store file.
///
/// A zero-length or all-whitespace file is *not* an empty store. This
/// crate only ever writes complete JSON documents, so a file like that
/// is almost certainly destroyed or truncated — it fails as
/// [`StoreError::DestroyedFile`] instead of quietly yielding an empty
/// map. Anything else must be a JSON array of [`OfflinePlayer`] records
/// with unique UUIDs. Duplicate UUIDs are a hard error carrying every
/// 0-based index at which the duplicated UUID appears.
fn parse_records(path: &Path, bytes: &[u8]) -> Result<BTreeMap<Uuid, OfflinePlayer>, StoreError> {
    // note: an empty slice passes the `all` check, so a zero-length file
    // and an all-whitespace file take the same failure path — which is
    // what we want, they are the same accident
    if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(StoreError::destroyed_file(
            path,
            "empty or all-whitespace (likely truncated or destroyed)",
        ));
    }

    let records: Vec<OfflinePlayer> =
        serde_json::from_slice(bytes).map_err(|source| StoreError::corrupt(path, source))?;

    // first pass: collect every index at which each uuid appears
    let mut indices: HashMap<Uuid, Vec<usize>> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        indices.entry(record.uuid).or_default().push(index);
    }
    // report the duplicate whose first occurrence comes earliest in the array
    let mut duplicates: Vec<(Uuid, Vec<usize>)> = indices
        .into_iter()
        .filter(|(_, indices)| indices.len() > 1)
        .collect();
    duplicates.sort_by_key(|(_, indices)| indices[0]);
    if let Some((uuid, indices)) = duplicates.into_iter().next() {
        return Err(StoreError::DuplicateUuid {
            path: path.to_path_buf(),
            uuid,
            indices,
        });
    }

    Ok(records
        .into_iter()
        .map(|record| (record.uuid, record))
        .collect())
}

/// Read and validate an existing store file. Missing file is handled by
/// the caller; this function assumes the file exists.
fn read_and_validate(path: &Path) -> Result<BTreeMap<Uuid, OfflinePlayer>, StoreError> {
    let metadata = std::fs::metadata(path).map_err(|source| StoreError::io(path, source))?;
    ensure_size_within_cap(path, metadata.len())?;
    let bytes = std::fs::read(path).map_err(|source| StoreError::io(path, source))?;
    parse_records(path, &bytes)
}

/// Serialize `players` and atomically replace `path` with the result.
///
/// Steps: write to `<path>.tmp-<pid>-<counter>` in the same directory,
/// `sync_all` the temporary file, then rename it over `path`. The rename
/// is the atomic step (same volume, so no cross-device fallback); if any
/// step fails the temporary file is removed best-effort so no `.tmp-*`
/// litter is left behind.
///
/// Before any of that, the target is checked and the save is refused as
/// [`StoreError::DestroyedFile`] when it exists but cannot be a store
/// this crate wrote: a zero-length file, a non-regular path (FIFO,
/// socket, device, dangling symlink), or anything the metadata call
/// cannot make sense of. This crate never writes an empty file, so such
/// a target is almost certainly a destroyed store — atomically replacing
/// it would erase the evidence. A missing target is fine, the rename
/// creates it. Note the check is metadata-only: an all-whitespace file
/// with `len > 0` slips past it (the full whitespace check lives in
/// `open()`), and that is a deliberate trade to keep saves cheap.
///
/// Directory fsync is deliberately skipped: the standard library has no
/// portable directory fsync on Windows, and the rename is what keeps
/// `players.json` from being torn open. Durability of the rename itself
/// against a full power loss is therefore best-effort, not guaranteed.
fn persist(path: &Path, players: &BTreeMap<Uuid, OfflinePlayer>) -> Result<(), StoreError> {
    // the parent directory may have been deleted since open(); recreate it
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|source| StoreError::io(parent, source))?;
    }

    // anti-clobber: never rename over a path that exists but cannot hold
    // a store this crate wrote (zero-length, non-regular, or unstatable).
    // a missing target is fine — the rename creates it
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            let (is_regular_file, is_zero_length) = if meta.file_type().is_symlink() {
                // follow the link; a dangling symlink is not a store
                match std::fs::metadata(path) {
                    Ok(followed) if followed.is_file() => (true, followed.len() == 0),
                    // symlink to a directory/device/socket, or a dangling
                    // one: not a regular file
                    _ => (false, false),
                }
            } else {
                (meta.is_file(), meta.is_file() && meta.len() == 0)
            };
            if !is_regular_file || is_zero_length {
                let detail = if is_regular_file {
                    // regular file, but zero-length: truncation signature
                    "empty or all-whitespace (likely truncated or destroyed)"
                } else {
                    "exists, but is not a regular file"
                };
                return Err(StoreError::destroyed_file(path, detail));
            }
        }
        // target is genuinely gone: the rename will create it
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        // cannot tell what is there (permissions, transient IO error):
        // be loud instead of clobbering
        Err(source) => return Err(StoreError::io(path, source)),
    }

    // serialize the full map as compact JSON; records come out in UUID
    // order (BTreeMap)
    let records: Vec<&OfflinePlayer> = players.values().collect();
    let bytes =
        serde_json::to_vec(&records).map_err(|source| StoreError::serialize(path, source))?;

    // tmp-<pid>-<counter>, unique within the process via TMP_COUNTER; a
    // stale file from an earlier run with a reused pid is overwritten
    let mut tmp_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    tmp_name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(tmp_name);

    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(source) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(StoreError::io(&tmp, source));
    }

    if let Err(source) = std::fs::rename(&tmp, path) {
        // do not leave the temporary file behind on failure
        let _ = std::fs::remove_file(&tmp);
        return Err(StoreError::io(path, source));
    }
    Ok(())
}

/// Insert `record` into `map`, persist the full map, and roll the map back
/// to its previous contents if persisting fails.
///
/// Keeping memory in sync with disk on failure means a caller that retries
/// after an error sees exactly what the file contains.
fn insert_and_persist(
    map: &mut BTreeMap<Uuid, OfflinePlayer>,
    path: &Path,
    record: OfflinePlayer,
) -> Result<OfflinePlayer, StoreError> {
    let uuid = record.uuid;
    let previous = map.insert(uuid, record.clone());
    if let Err(error) = persist(path, map) {
        match previous {
            Some(previous) => {
                map.insert(uuid, previous);
            }
            None => {
                map.remove(&uuid);
            }
        }
        return Err(error);
    }
    Ok(record)
}

/// A file-backed [`PlayerStore`] storing `OfflinePlayer` records in a
/// single JSON file (`<data_dir>/players.json` by convention).
///
/// # Format
///
/// One file holds a JSON array of [`OfflinePlayer`] records exactly as
/// `candyriya-player` serializes them — chrono RFC 3339 timestamps, UUID as
/// a hyphenated string, `properties` with an optional `signature` key that
/// is omitted for unsigned properties. The array is compact JSON on a
/// single line and records are written in UUID order, so the output is
/// deterministic for the same set of records. The file is rewritten
/// wholesale on every save; there is no log or append journal. Compact
/// JSON is not hand-editable and does not diff line-by-line — pretty-printed
/// files written by older versions still load, since the parser accepts
/// any JSON `serde_json` can read.
///
/// # Crash safety
///
/// Saves write to a temporary file in the same directory, `sync_all` it,
/// and then rename it over the target. A crash can lose the last save but
/// cannot leave `players.json` half-written. The parent directory is not
/// fsynced (no portable Windows API for it), so durability of the rename
/// itself against power loss is best-effort.
///
/// # Locking and concurrency
///
/// Within one process the in-memory map is behind a mutex, so concurrent
/// saves are sequential and each save atomically replaces the file. State
/// is shared process-wide; cloning this type is not provided. Multiple
/// *processes* writing the same file are out of scope: `players.json` is
/// meant to be owned by a single proxy instance and is not a shared
/// inter-process format. There is no inter-process file lock, so concurrent
/// writers from different processes can lose each other's data.
pub struct JsonPlayerStore {
    path: PathBuf,
    players: Mutex<BTreeMap<Uuid, OfflinePlayer>>,
}

impl fmt::Debug for JsonPlayerStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonPlayerStore")
            .field("path", &self.path)
            .field("players", &self.players.lock().len())
            .finish_non_exhaustive()
    }
}

impl JsonPlayerStore {
    /// Open the store at `path`.
    ///
    /// Creates the parent directory (and any missing ancestors) if it does
    /// not exist yet, so a fresh proxy with an empty data directory works
    /// without a setup step. A missing store file is *not* an error: it
    /// yields an empty map. An existing file is validated up front:
    ///
    /// - its size must not exceed [`MAX_FILE_SIZE_BYTES`];
    /// - every record must deserialize as an [`OfflinePlayer`];
    /// - UUIDs must be unique — duplicates are a hard error reporting
    ///   every 0-based index at which the duplicated UUID appears.
    ///
    /// A present-but-unusable file is a hard error, never silently
    /// adopted as a fresh store:
    ///
    /// - an empty or all-whitespace file is almost certainly destroyed or
    ///   truncated (this crate never writes one) —
    ///   [`StoreError::DestroyedFile`];
    /// - a path that exists but is not a regular file (FIFO, socket,
    ///   device, dangling symlink) — [`StoreError::DestroyedFile`];
    /// - a directory is an [`StoreError::Io`] with `IsADirectory`.
    ///
    /// Errors don't overwrite or truncate anything: a refused file is left
    /// untouched on disk, and the save path is guarded the same way, so a
    /// destroyed file is never atomically replaced either.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        if path.file_name().is_none() {
            return Err(StoreError::io(
                &path,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path does not name a file",
                ),
            ));
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::io(parent, source))?;
        }

        let players = if path.is_file() {
            read_and_validate(&path)?
        } else if path.is_dir() {
            return Err(StoreError::io(
                &path,
                std::io::Error::new(std::io::ErrorKind::IsADirectory, "path is a directory"),
            ));
        } else {
            match std::fs::symlink_metadata(&path) {
                // present (no-follow metadata says so), but not a regular
                // file or directory: FIFO, socket, device, dangling symlink.
                // dangling symlinks land here because is_file() follows
                // links and fails on them, so something IS on disk and
                // adopting a fresh store would silently erase it
                Ok(_) => {
                    return Err(StoreError::destroyed_file(
                        &path,
                        "exists, but is not a regular file",
                    ));
                }
                // genuinely missing: fresh store, nothing to destroy
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
                // cannot tell what's there (permissions, transient IO
                // error): be loud instead of guessing fresh
                Err(source) => return Err(StoreError::io(&path, source)),
            }
        };

        Ok(Self {
            path,
            players: Mutex::new(players),
        })
    }

    /// The path this store persists to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of records held (and, after a successful save, on disk).
    pub fn len(&self) -> usize {
        self.players.lock().len()
    }

    /// Whether the store holds no records.
    pub fn is_empty(&self) -> bool {
        self.players.lock().is_empty()
    }

    /// A snapshot of the full in-memory map (which is kept identical to
    /// the file after every successful save), in UUID order.
    pub fn load_all(&self) -> BTreeMap<Uuid, OfflinePlayer> {
        self.players.lock().clone()
    }

    /// Upsert the record for an online session: the proxy watched
    /// `profile` at `now`.
    ///
    /// Merge semantics match the in-memory store:
    ///
    /// - `profile.id` keys the record;
    /// - `first_seen` of an existing record is kept (first sighting wins);
    /// - `last_seen` becomes `now`, but never regresses — the maximum of
    ///   the stored value and `now` wins;
    /// - `name` and `properties` are overwritten with the latest sighting.
    ///
    /// Like [`PlayerStore::save`], this persists the full store atomically
    /// before returning; if persisting fails the in-memory map is rolled
    /// back so it never diverges from the file.
    pub fn upsert_from_session(
        &mut self,
        profile: &GameProfile,
        now: DateTime<Utc>,
    ) -> Result<OfflinePlayer, StoreError> {
        let mut map = self.players.lock();
        let record = match map.get(&profile.id) {
            Some(existing) => OfflinePlayer {
                uuid: existing.uuid,
                name: profile.name.clone(),
                properties: profile.properties.clone(),
                first_seen: existing.first_seen,
                last_seen: existing.last_seen.max(now),
            },
            None => OfflinePlayer::observed(profile, now),
        };
        insert_and_persist(&mut map, &self.path, record)
    }
}

impl PlayerStore for JsonPlayerStore {
    fn load(&self, uuid: Uuid) -> Result<Option<OfflinePlayer>, PlayerStoreError> {
        Ok(self.players.lock().get(&uuid).cloned())
    }

    /// Insert or replace the record for the player it describes and
    /// persist the full map atomically. Replacement is wholesale.
    ///
    /// If persisting fails the in-memory map is rolled back to its
    /// previous contents, so the store never reports a state the file
    /// does not contain.
    fn save(&mut self, player: OfflinePlayer) -> Result<(), PlayerStoreError> {
        let mut map = self.players.lock();
        insert_and_persist(&mut map, &self.path, player)
            .map(|_| ())
            .map_err(PlayerStoreError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_cap_accepts_exactly_cap() {
        let path = Path::new("players.json");
        assert!(ensure_size_within_cap(path, MAX_FILE_SIZE_BYTES).is_ok());
    }

    #[test]
    fn size_cap_rejects_one_byte_over() {
        let path = Path::new("players.json");
        let result = ensure_size_within_cap(path, MAX_FILE_SIZE_BYTES + 1);
        assert!(matches!(
            result,
            Err(StoreError::TooLarge { size, max, .. }) if size == MAX_FILE_SIZE_BYTES + 1 && max == MAX_FILE_SIZE_BYTES
        ));
    }

    #[test]
    fn whitespace_only_is_destroyed_error() {
        let path = Path::new("players.json");
        match parse_records(path, b"  \n\t\n") {
            Err(StoreError::DestroyedFile { path: got, detail }) => {
                assert_eq!(got, path);
                assert!(detail.contains("whitespace"));
            }
            other => panic!("expected DestroyedFile, got {other:?}"),
        }
    }

    #[test]
    fn zero_byte_is_destroyed_error() {
        let path = Path::new("players.json");
        match parse_records(path, b"") {
            Err(StoreError::DestroyedFile { path: got, detail }) => {
                assert_eq!(got, path);
                assert!(detail.contains("empty"));
            }
            other => panic!("expected DestroyedFile, got {other:?}"),
        }
    }
}
