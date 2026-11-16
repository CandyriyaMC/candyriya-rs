//! The error type for `candyriya-cache`.

/// Everything that can go wrong inside `candyriya-cache`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A plugin id failed the `[a-z0-9_.-]{1,32}` validation.
    #[error("invalid plugin id `{id}`: {reason}")]
    InvalidPluginId {
        /// The offending id, echoed for diagnostics.
        id: String,
        /// Why the id was rejected.
        reason: &'static str,
    },

    /// No data directory was given to the builder.
    #[error("a data directory is required")]
    MissingRoot,

    /// A failure coming from the underlying cachegirl registry.
    ///
    /// Quota violations surface as
    /// [`cachegirl::Error::Quota`](crate::CachegirlError::Quota), for
    /// example — match on this variant to tell the difference between a
    /// rejected plugin id and a namespace that is out of room.
    #[error("cachegirl: {0}")]
    Cachegirl(#[from] cachegirl::Error),
}

/// Result alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;
