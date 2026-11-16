//! Telemetry setup for the proxy: a [`tracing`] subscriber with a compact
//! text format, an optional daily-rotating file sink, and — only behind the
//! `otlp` cargo feature — an OTLP/gRPC (tonic) trace export pipeline.
//!
//! # What you get by default
//!
//! [`init`] with [`LogConfig::default`] installs a global, compact text
//! subscriber on stdout: UTC RFC 3339 timestamps, thread ids, levels,
//! targets, colors resolved from the [`Colors`] mode with `NO_COLOR` and
//! TTY detection. No network access of any kind happens, and nothing is
//! written to disk.
//!
//! # File logging
//!
//! [`LogConfig::with_file_rotation`] adds a non-blocking
//! [`tracing_appender`] file writer with daily rotation: files are named
//! `candyriya.log.YYYY-MM-DD` inside the given directory (created
//! recursively if missing). The console layer is ANSI-aware; the file layer
//! never emits ANSI escapes. The background writer thread is flushed and
//! stopped when the returned [`TelemetryGuard`] is dropped or
//! [`TelemetryGuard::shutdown`] is called — keep the guard alive until the
//! process is done logging.
//!
//! # `NO_COLOR`
//!
//! The presence of `NO_COLOR` (any value, including an empty one) in the
//! environment forces colors off for the console layer, regardless of
//! [`Colors`]. Files never get colors either way.
//!
//! # Levels
//!
//! The level string is parsed as an [`EnvFilter`], so per-target directives
//! such as `"info,candyriya=debug"` work. An invalid string falls back to
//! `info` (a warning is printed to stderr); initialization never fails
//! because of a bad level string.
//!
//! # OTLP (optional, OFF by default)
//!
//! The `otlp` feature (not enabled by default) wires an OTLP/gRPC span
//! pipeline on top of the fmt layers via `opentelemetry-otlp`'s `grpc-tonic`
//! backend. Provide [`OtlpExportConfig`] through [`LogConfig::with_otlp`].
//! Building or installing the exporter/provider is lazy about connecting: no
//! network traffic happens at install time. Any failure to build the
//! pipeline surfaces as [`TelemetryError::Otlp`], never as a panic — and the
//! call is still atomic with respect to the global subscriber slot.
//!
//! Note: the `otlp` path is compile-gated on the feature and is not covered
//! by local test runs; CI covers `--all-features`. The default build is
//! fmt-only.
//!
//! # Double initialization
//!
//! Only one [`init`] succeeds per process. A second call fails with
//! [`TelemetryError::AlreadyInitialized`] instead of panicking or silently
//! replacing the subscriber. (Internally `tracing` panics when a global
//! subscriber is set twice; this crate guards the slot itself and also maps
//! a foreign-already-installed subscriber onto a non-panicking error.)
//!
//! # Per-connection fields
//!
//! Proxy-level fields such as `conn_id` / `task_id` are the proxy's
//! business and are attached via `instrument` spans on the hot path; this
//! crate only provides the subscriber/layers they are emitted through.

#![forbid(unsafe_code)]

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Registry, fmt};

mod config;
mod error;
mod guard;
#[cfg(feature = "otlp")]
mod otlp_pipeline;

pub use crate::config::{Colors, LogConfig, OtlpExportConfig};
pub use crate::error::TelemetryError;
pub use crate::guard::TelemetryGuard;

static TELEMETRY_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initialize the global tracing subscriber from `config`.
///
/// Returns a [`TelemetryGuard`] that flushes the file writer and the OTLP
/// pipeline on drop/shutdown. See the [module-level docs](self) for the
/// behavioral contract, including double-initialization handling.
///
/// # Errors
///
/// - [`TelemetryError::AlreadyInitialized`] — [`init`] already succeeded in
///   this process (or a foreign global subscriber is installed).
/// - [`TelemetryError::SubscriberInstall`] — `tracing` refused the global
///   slot mid-install.
/// - [`TelemetryError::LogDirectory`] — the rotating-file directory could
///   not be created.
/// - [`TelemetryError::Otlp`] / [`TelemetryError::OtlpDisabled`] — OTLP
///   configuration invalid, or provided without the `otlp` feature.
pub fn init(config: &LogConfig) -> Result<TelemetryGuard, TelemetryError> {
    if TELEMETRY_INITIALIZED.swap(true, Ordering::AcqRel) {
        return Err(TelemetryError::AlreadyInitialized);
    }
    match install(config) {
        Ok(guard) => Ok(guard),
        Err(err) => {
            // Init failed; leave the slot free so a corrected call can retry.
            TELEMETRY_INITIALIZED.store(false, Ordering::Release);
            Err(err)
        }
    }
}

fn install(config: &LogConfig) -> Result<TelemetryGuard, TelemetryError> {
    // Reject an OTLP config up front in non-otlp builds.
    #[cfg(not(feature = "otlp"))]
    {
        if config.otlp().is_some() {
            return Err(TelemetryError::OtlpDisabled);
        }
    }
    if let Some(otlp) = config.otlp() {
        otlp.validate()?;
    }

    let no_color = std::env::var_os("NO_COLOR").is_some();
    let console_is_terminal = std::io::stdout().is_terminal();
    let ansi = config.colors_mode().resolve(no_color, console_is_terminal);

    let (filter, used_fallback) = env_filter(config.level_str());

    let registry = Registry::default().with(filter);

    let console_layer = config.console_enabled().then(|| {
        fmt::layer()
            .compact()
            .with_thread_ids(config.thread_ids_enabled())
            .with_level(true)
            .with_target(config.callsite_target_enabled())
            .with_timer(fmt::time::UtcTime::rfc_3339())
            .with_ansi(ansi)
            .with_writer(std::io::stdout)
    });

    let (file_layer, file_guard) = match config.file_rotation() {
        Some(dir) => {
            std::fs::create_dir_all(dir).map_err(|source| TelemetryError::LogDirectory {
                path: dir.to_path_buf(),
                source,
            })?;
            let appender = tracing_appender::rolling::daily(dir, "candyriya.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let layer = fmt::layer()
                .compact()
                .with_thread_ids(config.thread_ids_enabled())
                .with_level(true)
                .with_target(config.callsite_target_enabled())
                .with_timer(fmt::time::UtcTime::rfc_3339())
                .with_ansi(false)
                .with_writer(writer);
            (Some(layer), Some(guard))
        }
        None => (None, None),
    };

    #[cfg(feature = "otlp")]
    let (otel_layer, tracer_provider) = match config.otlp() {
        Some(otlp) => {
            let (layer, provider) = otlp_pipeline::build(otlp)?;
            (Some(layer), Some(provider))
        }
        None => (None, None),
    };

    let registry = registry.with(console_layer).with(file_layer);
    #[cfg(feature = "otlp")]
    let registry = registry.with(otel_layer);

    tracing::subscriber::set_global_default(registry)
        .map_err(|err| TelemetryError::SubscriberInstall(err.to_string()))?;

    if used_fallback {
        // Post-install notice: lands in the file as well when both sinks are on.
        tracing::warn!("invalid log directives, falling back to 'info'");
    }

    Ok(TelemetryGuard::new(
        file_guard,
        #[cfg(feature = "otlp")]
        tracer_provider,
    ))
}

/// Parse the directive string as an [`EnvFilter`], falling back to `info`.
/// Returns the filter and whether the fallback was used.
fn env_filter(level: &str) -> (EnvFilter, bool) {
    let directive = level.trim();
    match EnvFilter::try_new(directive) {
        Ok(filter) if !directive.is_empty() => (filter, false),
        _ => {
            eprintln!(
                "candyriya-telemetry: invalid log directives '{directive}', falling back to 'info'"
            );
            (EnvFilter::new("info"), true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_filter_parses_directives() {
        let (filter, fallback) = env_filter("info");
        assert!(!fallback);
        let _ = filter;

        let (_, fallback) = env_filter("info,candyriya=debug");
        assert!(!fallback);

        let (_, fallback) = env_filter("candyriya=debug");
        assert!(!fallback);
    }

    #[test]
    fn env_filter_falls_back_on_garbage() {
        // Premise: the string really is rejected, otherwise the fallback branch is dead code.
        assert!(EnvFilter::try_new("candyriya=notalevel").is_err());

        let (_, fallback) = env_filter("candyriya=notalevel");
        assert!(fallback);

        let (_, fallback) = env_filter("");
        assert!(fallback);

        let (_, fallback) = env_filter("   ");
        assert!(fallback);
    }
}
