//! Canonical atomic file write.
//!
//! Every crate that persists state (player store, config dumps, future cache
//! snapshots) should use [`atomic_write`] instead of hand-rolled
//! write-then-rename so the semantics stay identical everywhere: a reader
//! either sees the complete old file or the complete new one, never a torn
//! write.
//!
//! How it works: the payload is written to `<path>.tmp-<pid>-<counter>` in the
//! same directory, the temporary file is `sync_all`ed, then renamed over
//! `path`. The rename is the atomic step (same volume, so no cross-device
//! fallback). If any step fails the temporary file is removed best-effort, so
//! no `.tmp-*` litter is left behind.
//!
//! Parent directories of `path` are created if missing. A process-wide counter
//! plus the pid keeps temporary names unique within a process; names from an
//! unrelated crashed run with a reused pid may be overwritten, which is safe
//! because they carry no data yet.
//!
//! Not provided on purpose: directory fsync. The standard library has no
//! portable directory sync on Windows, so durability of the rename itself
//! against a full power loss is best-effort. The rename does guarantee atomic
//! visibility for readers on the same filesystem.

use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` atomically.
///
/// `path` is replaced with `bytes` or left untouched if anything fails
/// (spawn of the temporary file, partial write, rename). Parent directories
/// are created as needed.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use candyriya_utils::atomic_write;
///
/// atomic_write(b"{\"ok\":true}", Path::new("data/state.json")).unwrap();
/// ```
pub fn atomic_write(bytes: &[u8], path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)?;
    }

    // tmp-<pid>-<counter>, unique within the process via TMP_COUNTER
    let mut tmp_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "candyriya".into());
    tmp_name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(tmp_name);

    let write_result = (|| -> io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(source) = write_result {
        // best effort: do not leave the temporary file behind
        let _ = std::fs::remove_file(&tmp);
        return Err(source);
    }

    if let Err(source) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(source);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_write_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let payload = b"{\"players\":[]}";
        atomic_write(payload, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), payload);
    }

    #[test]
    fn overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.bin");
        atomic_write(b"old", &path).unwrap();
        atomic_write(b"new-and-longer-payload", &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new-and-longer-payload");
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c").join("file.txt");
        atomic_write(b"nested", &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"nested");
    }

    #[test]
    fn no_tmp_litter_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        atomic_write(b"payload", &path).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries, vec!["state.json".to_string()]);
    }

    #[test]
    fn no_tmp_litter_on_rename_failure() {
        let dir = tempfile::tempdir().unwrap();
        // a directory at the target path makes the rename fail on both
        // Windows (file -> directory) and Unix (EISDIR)
        let path = dir.path().join("state.json");
        std::fs::create_dir(&path).unwrap();
        let result = atomic_write(b"payload", &path);
        assert!(result.is_err());
        let litter: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(litter.is_empty(), "temporary files left behind: {litter:?}");
    }

    #[test]
    fn empty_payload_writes_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        atomic_write(&[], &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), Vec::<u8>::new());
    }
}
