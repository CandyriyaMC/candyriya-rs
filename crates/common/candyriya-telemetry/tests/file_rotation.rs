//! The daily-rotating file sink creates `candyriya.log.YYYY-MM-DD`
//! inside the configured directory.

use candyriya_telemetry::{LogConfig, init};

#[test]
fn rotating_file_is_created_in_the_given_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_dir = dir.path().join("logs");

    let guard = init(
        &LogConfig::default()
            .with_file_rotation(&log_dir)
            .without_console(),
    )
    .expect("init with file rotation must succeed");

    tracing::info!("file rotation smoke test");
    drop(guard);

    let entries: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap_or_else(|err| panic!("log dir {} must exist after init: {err}", log_dir.display()))
        .filter_map(|entry| entry.ok())
        .collect();
    let has_log = entries.iter().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("candyriya.log"))
    });
    assert!(
        has_log,
        "expected a file starting with 'candyriya.log' in {}, found: {:?}",
        log_dir.display(),
        entries
            .iter()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    );
}
