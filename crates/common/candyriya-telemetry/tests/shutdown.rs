//! Explicit `shutdown` consumes the guard without panicking.

use candyriya_telemetry::{LogConfig, init};

#[test]
fn explicit_shutdown_consumes_the_guard() {
    let guard = init(&LogConfig::default().without_console()).expect("init must succeed");
    guard.shutdown();
    // Nothing left to assert: the absence of a panic on shutdown is the test.
}
