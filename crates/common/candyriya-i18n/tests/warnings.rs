//! Isolated warning-capture tests. Kept in a separate binary so the global
//! tracing subscriber cannot race other tests in the same process; each
//! target runs in its own process image.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use candyriya_i18n::I18n;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir()
            .join(format!("candyriya-i18n-warnings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir must be created");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn write_file(&self, name: &str, contents: &str) {
        let file_path = self.0.join(name);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).expect("temp dir structure must be created");
        }
        std::fs::write(&file_path, contents).expect("test file must be written");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capture_warns() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(CaptureWriter(Arc::clone(&buf)))
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (buf, guard)
}

fn captured_text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(buf.lock().unwrap_or_else(|p| p.into_inner()).clone())
        .expect("captured log is utf-8")
}

#[test]
fn broken_override_file_falls_back_with_warning() {
    let tmp = TempDir::new("broken-warn");
    tmp.write_file("locales/en/common.toml", "not toml at all [");
    let (buf, _guard) = capture_warns();
    let _i18n = I18n::with_override_dir("en", &tmp.path().join("locales"))
        .expect("a broken file must not fail the whole load");
    let logs = captured_text(&buf);
    assert!(
        logs.contains("common.toml"),
        "warning must name the offending file, got: {logs}"
    );
    assert!(
        logs.contains("falling back"),
        "warning must say we fall back, got: {logs}"
    );
}

