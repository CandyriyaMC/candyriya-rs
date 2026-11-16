//! An invalid level string must not panic or fail initialization:
//! it falls back to `info`.

use candyriya_telemetry::{LogConfig, init};

#[test]
fn invalid_level_falls_back_to_info() {
    let guard = init(
        &LogConfig::default()
            .level("candyriya=notalevel")
            .without_console(),
    )
    .expect("invalid level must fall back to info, not fail");
    tracing::info!("post-fallback smoke test");
    drop(guard);
}
