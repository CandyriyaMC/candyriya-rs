//! The default build has no OTLP support: attaching an OTLP config fails
//! with `OtlpDisabled` instead of panicking or silently doing nothing.
//!
//! Both scenarios live in one test because this file is a single process:
//! `init` only has one global slot, and failed installs reset it.

#![cfg(not(feature = "otlp"))]

use candyriya_telemetry::{LogConfig, OtlpExportConfig, TelemetryError, init};

#[test]
fn otlp_config_without_the_feature_errors() {
    let otlp = OtlpExportConfig::new("http://localhost:4317", "candyriya", 0.5)
        .expect("valid otlp config");

    // Builder path.
    let err = init(
        &LogConfig::default()
            .with_otlp(otlp.clone())
            .without_console(),
    )
    .expect_err("otlp without the feature must error");
    assert!(
        matches!(err, TelemetryError::OtlpDisabled),
        "expected OtlpDisabled, got {err:?}"
    );

    // from_otel constructor path (the failed install above released the slot).
    let err = init(&LogConfig::from_otel(otlp).without_console())
        .expect_err("otlp without the feature must error");
    assert!(
        matches!(err, TelemetryError::OtlpDisabled),
        "expected OtlpDisabled, got {err:?}"
    );
}
