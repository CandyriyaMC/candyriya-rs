//! The guard returned by [`crate::init`].

use std::fmt;

/// Drop guard for the installed telemetry pipeline.
///
/// On drop — or via an explicit [`Self::shutdown`] call — this flushes and
/// stops the background file-writer thread (if any) and shuts down the
/// OpenTelemetry tracer provider (if any) so buffered spans are exported
/// before the process exits.
///
/// Note: the global subscriber installed by [`crate::init`] is never
/// uninstalled (tracing exposes no API for that); the guard governs only
/// the auxiliary resources — the non-blocking file writer and the OTLP
/// tracer provider. Keep it alive for as long as logging must keep working,
/// and drop it before the process exits.
pub struct TelemetryGuard {
    file_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
    #[cfg(feature = "otlp")]
    tracer_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl TelemetryGuard {
    pub(crate) fn new(
        file_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
        #[cfg(feature = "otlp")] tracer_provider: Option<
            opentelemetry_sdk::trace::SdkTracerProvider,
        >,
    ) -> Self {
        Self {
            file_guard,
            #[cfg(feature = "otlp")]
            tracer_provider,
        }
    }

    /// Shut the pipeline down explicitly.
    ///
    /// Semantics are identical to dropping the guard; consume the guard so
    /// it cannot be used afterwards. Prefer this at the end of the proxy's
    /// graceful shutdown sequence.
    pub fn shutdown(self) {
        // Drop does all the work.
    }
}

impl fmt::Debug for TelemetryGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("TelemetryGuard");
        debug.field("file_rotation", &self.file_guard.is_some());
        #[cfg(feature = "otlp")]
        debug.field("otlp", &self.tracer_provider.is_some());
        debug.finish()
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        #[cfg(feature = "otlp")]
        if let Some(provider) = self.tracer_provider.take() {
            // Best-effort flush of buffered spans. The global subscriber is
            // still installed at this point, but by now the proxy is on its
            // way down; report through stderr rather than through tracing.
            if let Err(err) = provider.shutdown() {
                eprintln!(
                    "candyriya-telemetry: failed to flush the OpenTelemetry tracer provider on shutdown: {err}"
                );
            }
        }
        // Dropping the WorkerGuard closes the channel to the background
        // writer thread, which flushes any buffered log lines and exits.
        drop(self.file_guard.take());
    }
}
