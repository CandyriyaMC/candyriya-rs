//! Error type for the telemetry setup.

use std::path::PathBuf;

/// Errors returned by [`crate::init`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TelemetryError {
    /// Something was already initialized: either [`crate::init`] was called
    /// twice in the same process, or another global tracing subscriber had
    /// been installed before us.
    #[error("telemetry was already initialized in this process")]
    AlreadyInitialized,

    /// The global subscriber could not be installed because a foreign
    /// subscriber was already registered with `tracing`.
    #[error("failed to install the global tracing subscriber: {0}")]
    SubscriberInstall(String),

    /// The configured rotating-file directory could not be created.
    #[error("failed to create the log directory {path}: {source}")]
    LogDirectory {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// The provided OTLP configuration is invalid, or the exporter/pipeline
    /// could not be built. Never a panic: OTLP install failure is an error.
    #[error("invalid OTLP configuration: {0}")]
    Otlp(String),

    /// An OTLP configuration was provided, but the crate was built without
    /// the `otlp` feature, so there is nothing to install it with.
    #[error(
        "an OTLP configuration was provided, but the `otlp` feature is not enabled in this build"
    )]
    OtlpDisabled,
}
