//! Builder configuration for the telemetry setup.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// When to emit ANSI colors in console output.
///
/// The `NO_COLOR` environment variable (presence-based, any value including
/// an empty one) always wins over this setting: colors are suppressed, even
/// for [`Colors::Always`]. See <https://no-color.org/>.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Colors {
    /// Emit colors only when stdout is a terminal (and `NO_COLOR` is not set).
    #[default]
    Auto,
    /// Always emit colors. `NO_COLOR` still wins over this setting.
    Always,
    /// Never emit colors.
    Never,
}

impl Colors {
    /// Resolve the effective ANSI decision from the color mode, the `NO_COLOR`
    /// presence check and the console's terminal state.
    ///
    /// Kept as a pure function so the decision matrix is unit-testable
    /// without touching the environment or the real TTY.
    #[must_use]
    pub(crate) const fn resolve(self, no_color: bool, console_is_terminal: bool) -> bool {
        if no_color {
            return false;
        }
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::Auto => console_is_terminal,
        }
    }
}

/// Configuration for the optional OTLP trace export pipeline.
///
/// This is an API shim: it exists in every build so callers can construct
/// and pass it uniformly. The pipeline itself is only wired up when the
/// crate is built with the `otlp` feature; the default build performs no
/// network I/O at all and rejects an attached OTLP config with
/// [`crate::TelemetryError::OtlpDisabled`].
#[derive(Debug, Clone, PartialEq)]
pub struct OtlpExportConfig {
    /// gRPC endpoint of the collector, e.g. `http://localhost:4317`.
    pub endpoint: String,
    /// Service name reported as the `service.name` resource attribute.
    pub service_name: String,
    /// Fraction of traces to sample, in the closed interval `[0.0, 1.0]`.
    pub sample_ratio: f64,
}

impl OtlpExportConfig {
    /// Create a new OTLP export configuration, validating the inputs.
    ///
    /// # Errors
    ///
    /// Returns [`crate::TelemetryError::Otlp`] when the endpoint or service
    /// name are blank, or when `sample_ratio` is not a finite number in
    /// `[0.0, 1.0]`.
    pub fn new(
        endpoint: impl Into<String>,
        service_name: impl Into<String>,
        sample_ratio: f64,
    ) -> Result<Self, crate::TelemetryError> {
        let config = Self {
            endpoint: endpoint.into(),
            service_name: service_name.into(),
            sample_ratio,
        };
        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration. Cheap; run again at install time.
    pub(crate) fn validate(&self) -> Result<(), crate::TelemetryError> {
        if self.endpoint.trim().is_empty() {
            return Err(crate::TelemetryError::Otlp(
                "endpoint must not be empty".to_string(),
            ));
        }
        if self.service_name.trim().is_empty() {
            return Err(crate::TelemetryError::Otlp(
                "service_name must not be empty".to_string(),
            ));
        }
        if !self.sample_ratio.is_finite() || !(0.0..=1.0).contains(&self.sample_ratio) {
            return Err(crate::TelemetryError::Otlp(format!(
                "sample_ratio must be a finite number in [0.0, 1.0], got {}",
                self.sample_ratio
            )));
        }
        Ok(())
    }
}

/// Logging and tracing configuration for the proxy.
///
/// Builder-style API: methods consume `self` and return `Self`.
///
/// Defaults: `info` level (parsed as an `EnvFilter` directive list, so
/// per-target directives like `"info,candyriya=debug"` work),
/// [`Colors::Auto`], console output on stdout, thread ids on, event targets
/// on, no file logging, no OTLP.
#[derive(Debug, Clone)]
pub struct LogConfig {
    level: String,
    colors: Colors,
    console: bool,
    thread_ids: bool,
    callsite_target: bool,
    file_rotation: Option<PathBuf>,
    otlp: Option<OtlpExportConfig>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            colors: Colors::Auto,
            console: true,
            thread_ids: true,
            callsite_target: true,
            file_rotation: None,
            otlp: None,
        }
    }
}

impl LogConfig {
    /// Create a configuration with the defaults:
    /// `info` level, auto colors, console on stdout, thread ids and targets
    /// on, no file logging, no OTLP.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the level/directive string, parsed as an `EnvFilter`.
    ///
    /// Supports per-target directives, e.g. `"info,candyriya=debug"`. An
    /// invalid string falls back to `info` at install time and a warning is
    /// printed; it never aborts initialization.
    #[must_use]
    pub fn level(mut self, level: impl Into<String>) -> Self {
        self.level = level.into();
        self
    }

    /// Set the console color mode. `NO_COLOR` (when present in the
    /// environment) overrides this setting.
    #[must_use]
    pub fn colors(mut self, colors: Colors) -> Self {
        self.colors = colors;
        self
    }

    /// Enable console logging on stdout. This is the default.
    #[must_use]
    pub fn with_console(mut self) -> Self {
        self.console = true;
        self
    }

    /// Disable console logging.
    #[must_use]
    pub fn without_console(mut self) -> Self {
        self.console = false;
        self
    }

    /// Log to a daily-rotating file inside `dir`.
    ///
    /// Files are named `candyriya.log.YYYY-MM-DD` (tracing-appender
    /// `rolling::daily` with the `candyriya.log` prefix). The directory is
    /// created recursively at install time if missing; failure to create it
    /// is [`crate::TelemetryError::LogDirectory`]. Writes go through a
    /// non-blocking background writer; see [`crate::TelemetryGuard`].
    #[must_use]
    pub fn with_file_rotation(mut self, dir: impl Into<PathBuf>) -> Self {
        self.file_rotation = Some(dir.into());
        self
    }

    /// Include the thread id in each log line. This is the default.
    #[must_use]
    pub fn with_thread_ids(mut self) -> Self {
        self.thread_ids = true;
        self
    }

    /// Omit the thread id from log lines.
    #[must_use]
    pub fn without_thread_ids(mut self) -> Self {
        self.thread_ids = false;
        self
    }

    /// Include the event's target in each line.
    ///
    /// The "callsite target" is the module path where the event was
    /// recorded, e.g. `candyriya_proxy::accept` (this toggles `with_target`
    /// on the fmt layer). This is the default. File names and line numbers
    /// are intentionally never printed.
    #[must_use]
    pub fn with_callsite_target(mut self) -> Self {
        self.callsite_target = true;
        self
    }

    /// Omit the event target from log lines.
    #[must_use]
    pub fn without_callsite_target(mut self) -> Self {
        self.callsite_target = false;
        self
    }

    /// Attach an OTLP trace export configuration.
    ///
    /// Only wired up when the crate is built with the `otlp` feature;
    /// otherwise [`crate::init`] fails with
    /// [`crate::TelemetryError::OtlpDisabled`].
    #[must_use]
    pub fn with_otlp(mut self, otlp: OtlpExportConfig) -> Self {
        self.otlp = Some(otlp);
        self
    }

    /// Convenience constructor equivalent to
    /// `LogConfig::default().with_otlp(otlp)`.
    ///
    /// This takes the place of the separate `TelemetryConfig::from_otel`
    /// from the original sketch: the crate has exactly one config type, and
    /// a second, OTLP-only one would be a redundant parallel hierarchy.
    #[must_use]
    pub fn from_otel(otlp: OtlpExportConfig) -> Self {
        Self::default().with_otlp(otlp)
    }

    /// The configured level/directive string.
    #[must_use]
    pub fn level_str(&self) -> &str {
        &self.level
    }

    /// The configured console color mode.
    #[must_use]
    pub fn colors_mode(&self) -> Colors {
        self.colors
    }

    /// Whether console logging is enabled.
    #[must_use]
    pub fn console_enabled(&self) -> bool {
        self.console
    }

    /// Whether thread ids are printed.
    #[must_use]
    pub fn thread_ids_enabled(&self) -> bool {
        self.thread_ids
    }

    /// Whether the event target is printed.
    #[must_use]
    pub fn callsite_target_enabled(&self) -> bool {
        self.callsite_target
    }

    /// The configured rotating-file directory, if any.
    #[must_use]
    pub fn file_rotation(&self) -> Option<&Path> {
        self.file_rotation.as_deref()
    }

    /// The attached OTLP configuration, if any.
    #[must_use]
    pub fn otlp(&self) -> Option<&OtlpExportConfig> {
        self.otlp.as_ref()
    }

    /// The effective ANSI decision for this process right now.
    ///
    /// `NO_COLOR` (presence-based) forces colors off; otherwise resolved
    /// from [`Self::colors_mode`] and TTY detection on stdout. Exposed so
    /// callers can report the decision without installing the subscriber.
    #[must_use]
    pub fn effective_ansi(&self) -> bool {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        let console_is_terminal = std::io::stdout().is_terminal();
        self.colors.resolve(no_color, console_is_terminal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let config = LogConfig::default();
        assert_eq!(config.level_str(), "info");
        assert_eq!(config.colors_mode(), Colors::Auto);
        assert!(config.console_enabled());
        assert!(config.thread_ids_enabled());
        assert!(config.callsite_target_enabled());
        assert_eq!(config.file_rotation(), None);
        assert_eq!(config.otlp(), None);
    }

    #[test]
    fn builder_roundtrip() {
        let config = LogConfig::new()
            .level("info,candyriya=debug")
            .colors(Colors::Never)
            .without_console()
            .with_console()
            .with_thread_ids()
            .without_thread_ids()
            .with_callsite_target()
            .without_callsite_target()
            .with_file_rotation("logs");
        assert_eq!(config.level_str(), "info,candyriya=debug");
        assert_eq!(config.colors_mode(), Colors::Never);
        assert!(config.console_enabled());
        assert!(!config.thread_ids_enabled());
        assert!(!config.callsite_target_enabled());
        assert_eq!(config.file_rotation(), Some(Path::new("logs")));
    }

    #[test]
    fn resolve_ansi_decision_matrix() {
        // (mode, NO_COLOR present, stdout is a TTY) -> ANSI?
        let cases = [
            (Colors::Auto, false, true, true),
            (Colors::Auto, false, false, false),
            (Colors::Auto, true, true, false),
            (Colors::Auto, true, false, false),
            (Colors::Always, false, true, true),
            (Colors::Always, false, false, true),
            (Colors::Always, true, true, false),
            (Colors::Always, true, false, false),
            (Colors::Never, false, true, false),
            (Colors::Never, false, false, false),
            (Colors::Never, true, true, false),
            (Colors::Never, true, false, false),
        ];
        for (mode, no_color, tty, expected) in cases {
            assert_eq!(
                mode.resolve(no_color, tty),
                expected,
                "mode {mode:?}, no_color {no_color}, tty {tty}"
            );
        }
    }

    #[test]
    fn otlp_config_validation() {
        assert!(OtlpExportConfig::new("http://localhost:4317", "candyriya", 0.5).is_ok());
        assert!(OtlpExportConfig::new("http://localhost:4317", "candyriya", 0.0).is_ok());
        assert!(OtlpExportConfig::new("http://localhost:4317", "candyriya", 1.0).is_ok());
        assert!(OtlpExportConfig::new("", "candyriya", 0.5).is_err());
        assert!(OtlpExportConfig::new("   ", "candyriya", 0.5).is_err());
        assert!(OtlpExportConfig::new("http://localhost:4317", "", 0.5).is_err());
        assert!(OtlpExportConfig::new("http://localhost:4317", "candyriya", f64::NAN).is_err());
        assert!(
            OtlpExportConfig::new("http://localhost:4317", "candyriya", f64::INFINITY).is_err()
        );
        assert!(OtlpExportConfig::new("http://localhost:4317", "candyriya", -0.1).is_err());
        assert!(OtlpExportConfig::new("http://localhost:4317", "candyriya", 1.1).is_err());
    }

    #[test]
    fn from_otel_attaches_config() {
        let otlp = OtlpExportConfig::new("http://collector:4317", "candyriya", 0.25).unwrap();
        let config = LogConfig::from_otel(otlp.clone());
        assert_eq!(config.otlp(), Some(&otlp));
        assert!(config.console_enabled());
        assert_eq!(config.level_str(), "info");
    }
}
