//! Records written before the guard is dropped must reach the file:
//! dropping the guard flushes the background writer.

use candyriya_telemetry::{LogConfig, init};

#[test]
fn drop_flushes_buffered_records_to_file() {
    const RECORDS: usize = 100;
    const MARKER: &str = "flush-canary-ping";

    let dir = tempfile::tempdir().expect("tempdir");

    let guard = init(
        &LogConfig::default()
            .with_file_rotation(dir.path())
            .without_console(),
    )
    .expect("init with file rotation must succeed");

    for i in 0..RECORDS {
        tracing::info!(record = i, "{MARKER} {i}");
    }
    drop(guard);

    let mut contents = String::new();
    for entry in std::fs::read_dir(dir.path())
        .expect("log dir must exist")
        .flatten()
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("candyriya.log") {
            contents.push_str(
                &std::fs::read_to_string(entry.path()).expect("log file must be readable"),
            );
        }
    }

    let occurrences = contents.matches(MARKER).count();
    assert_eq!(
        occurrences, RECORDS,
        "expected {RECORDS} canary lines in the log files, found {occurrences}"
    );
}
