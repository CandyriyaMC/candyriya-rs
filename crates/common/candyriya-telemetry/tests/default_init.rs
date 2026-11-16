//! Default initialization works and the guard drops cleanly.

use candyriya_telemetry::{LogConfig, init};

#[test]
fn default_config_inits_and_drops() {
    let guard = init(&LogConfig::default()).expect("default init must succeed");
    tracing::info!("default subscriber smoke test");
    drop(guard);
}
