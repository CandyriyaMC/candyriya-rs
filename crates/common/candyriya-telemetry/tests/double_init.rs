//! A second `init` in the same process fails with `AlreadyInitialized`
//! instead of panicking.

use candyriya_telemetry::{LogConfig, TelemetryError, init};

#[test]
fn second_init_errors() {
    let _guard = init(&LogConfig::default().without_console()).expect("first init must succeed");
    let err = init(&LogConfig::default().without_console()).expect_err("second init must fail");
    assert!(
        matches!(err, TelemetryError::AlreadyInitialized),
        "expected AlreadyInitialized, got {err:?}"
    );
}
