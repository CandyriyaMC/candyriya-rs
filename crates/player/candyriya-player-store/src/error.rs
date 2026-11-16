//! Errors reported by [`crate::JsonPlayerStore`].

use std::path::PathBuf;

use uuid::Uuid;

/// Errors a [`crate::JsonPlayerStore`] can report.
///
/// Every variant carries the offending path so operators can find the file
/// that needs attention without guessing which store instance failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A filesystem operation failed (read, create, write, sync or rename).
    #[error("player store I/O error at '{}': {source}", .path.display())]
    Io {
        /// The file the operation was attempted on.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The backing file exceeds the admitted size cap, which means the file
    /// is almost certainly not a store written by this crate (or is
    /// corrupt). Refused before reading to avoid allocating unbounded
    /// buffers on hostile or accidentally huge files.
    #[error(
        "player store file '{}' is too large: {size} bytes (cap is {max} bytes)",
        .path.display()
    )]
    TooLarge {
        /// The file that was refused.
        path: PathBuf,
        /// The actual file size in bytes.
        size: u64,
        /// The configured cap in bytes.
        max: u64,
    },

    /// The backing file is not a JSON array of [`candyriya_player::OfflinePlayer`]
    /// records, or one of the records failed to deserialize.
    #[error("player store file '{}' failed to parse: {source}", .path.display())]
    Corrupt {
        /// The file that failed to parse.
        path: PathBuf,
        /// The JSON decoding error (includes line/column when available).
        #[source]
        source: serde_json::Error,
    },

    /// Serializing the in-memory records back to JSON failed. In practice
    /// this cannot happen for [`candyriya_player::OfflinePlayer`] — the
    /// variant exists so API paths never unwrap.
    #[error("player store file '{}' failed to serialize: {source}", .path.display())]
    Serialize {
        /// The file the records were being written to.
        path: PathBuf,
        /// The JSON encoding error.
        #[source]
        source: serde_json::Error,
    },

    /// Two records in the backing file carry the same UUID. Store files are
    /// keyed by UUID, so duplicates make the file ambiguous and are a hard
    /// error. Carries every index (0-based, array order) at which the UUID
    /// appears so operators can find and fix the duplicates.
    #[error(
        "player store file '{}' contains duplicate uuid {uuid} at indices {indices:?}",
        .path.display()
    )]
    DuplicateUuid {
        /// The file that contained the duplicate.
        path: PathBuf,
        /// The duplicated UUID.
        uuid: Uuid,
        /// All 0-based indices (array order) where the UUID appears.
        indices: Vec<usize>,
    },
}

impl StoreError {
    /// Wrap a filesystem error with the path it happened on.
    pub(crate) fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Wrap a parse failure with the path it happened on.
    pub(crate) fn corrupt(path: &std::path::Path, source: serde_json::Error) -> Self {
        Self::Corrupt {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Wrap a serialization failure with the path it happened on.
    pub(crate) fn serialize(path: &std::path::Path, source: serde_json::Error) -> Self {
        Self::Serialize {
            path: path.to_path_buf(),
            source,
        }
    }
}

impl From<StoreError> for candyriya_player::PlayerStoreError {
    fn from(error: StoreError) -> Self {
        // PlayerStoreError only has a human-readable Backend detail string;
        // fold the rich error into it instead of losing information.
        candyriya_player::PlayerStoreError::Backend(error.to_string())
    }
}
