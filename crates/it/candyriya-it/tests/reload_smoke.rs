//! Reload smoke test: the real `candyriya` binary, a temporary data
//! directory and a temporary configuration, driven through a pty.
//!
//! **Off by default**: the test only runs when `CANDYRIYA_SMOKE=1` is set
//! in the environment (the same gate the Paper smoke uses). Nothing
//! downloads, nothing binds outside loopback, and the run directory under
//! `target/smoke/` is throwaway. What it proves: `/candyriya reload`
//! answers over the console without crashing, prints both phase summaries,
//! and the proxy still exits 0 on `/candyriya end`.
//!
//! Pixel-perfect output is not asserted (colors and terminal sizes vary);
//! substring presence is.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const PROXY_ADDR: &str = "127.0.0.1:25571";
const PROXY_PORT: u16 = 25571;
const PROXY_READY_TIMEOUT: Duration = Duration::from_secs(30);
const RELOAD_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for ancestor in manifest.ancestors() {
        if ancestor.join("Cargo.toml").is_file() && ancestor.join("crates").is_dir() {
            return ancestor.to_path_buf();
        }
    }
    panic!(
        "could not locate the workspace root from {}",
        manifest.display()
    );
}

fn proxy_binary() -> PathBuf {
    if let Some(explicit) = std::env::var_os("CANDYRIYA_SMOKE_BIN") {
        let path = PathBuf::from(explicit);
        assert!(
            path.is_file(),
            "CANDYRIYA_SMOKE_BIN points at {}, which is not a file",
            path.display()
        );
        return path;
    }
    let root = workspace_root();
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    for profile in ["debug", "release"] {
        let candidate = root
            .join("target")
            .join(profile)
            .join(format!("candyriya{suffix}"));
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "no candyriya binary found; build it first with `cargo build -p candyriya` \
         or point CANDYRIYA_SMOKE_BIN at one"
    );
}

/// A child process's captured output, shareable with reader threads.
#[derive(Clone)]
struct Log {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Log {
    fn spawn_reader<R: Read + Send + 'static>(reader: R) -> Self {
        let log = Self {
            bytes: Arc::new(Mutex::new(Vec::new())),
        };
        let shared = Arc::clone(&log.bytes);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => match shared.lock() {
                        Ok(mut bytes) => bytes.extend_from_slice(&buf[..n]),
                        Err(_) => return,
                    },
                }
            }
        });
        log
    }

    fn text(&self) -> String {
        match self.bytes.lock() {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => String::from("<log lost to a poisoned mutex>"),
        }
    }

    fn contains(&self, needle: &str) -> bool {
        self.text().contains(needle)
    }

    fn count_occurrences(&self, needle: &[u8]) -> usize {
        let bytes = self.bytes.lock().map(|b| b.clone()).unwrap_or_default();
        if needle.is_empty() || bytes.len() < needle.len() {
            return 0;
        }
        bytes
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }
}

fn wait_for<F: FnMut() -> bool>(timeout: Duration, poll: Duration, what: &str, mut condition: F) {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} (after {timeout:?})"
        );
        std::thread::sleep(poll);
    }
}

// console-aware ANSI writers query the terminal with ESC[6n at startup;
// a pty that never answers leaves the child stuck before the first line
fn spawn_dsr_responder(log: Log, writer: Arc<Mutex<Box<dyn Write + Send>>>) {
    const DSR: &[u8] = b"\x1b[6n";
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut answered = 0usize;
        while Instant::now() < deadline {
            let pending = log.count_occurrences(DSR).saturating_sub(answered);
            if pending > 0 {
                if let Ok(mut writer) = writer.lock() {
                    for _ in 0..pending {
                        let _ = writer.write_all(b"\x1b[1;1R");
                    }
                }
                answered += pending;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
}

#[test]
fn reload_answers_over_the_console() {
    if std::env::var_os("CANDYRIYA_SMOKE").is_none() {
        eprintln!(
            "skipping the reload smoke test: set CANDYRIYA_SMOKE=1 to run it \
             (see crates/it/candyriya-it/README.md)"
        );
        return;
    }

    let root = workspace_root();
    let work = root.join("target").join("smoke").join("reload");
    if work.exists() {
        std::fs::remove_dir_all(&work).expect("clearing the previous reload smoke run");
    }
    std::fs::create_dir_all(&work).expect("creating the reload smoke run directory");

    let config = format!(
        r#"# Generated by the reload smoke test on every run; never committed.
config-version = "0.1"
bind = "{PROXY_ADDR}"
motd = "<green>reload smoke"
show-max-players = 8
online-mode = false
player-info-forwarding-mode = "disabled"

[protocol]
# the smoke must not depend on GitHub being reachable
auto-update = false
"#
    );
    let config_path = work.join("candyriya.toml");
    std::fs::write(&config_path, config).expect("writing the smoke candyriya.toml");

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("opening a pseudo-terminal");
    let mut command = CommandBuilder::new(proxy_binary());
    command.arg("--config");
    command.arg(&config_path);
    command.cwd(&work);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawning candyriya on the pty");
    drop(pair.slave);
    let proxy_log = Log::spawn_reader(pair.master.try_clone_reader().expect("pty reader"));
    let proxy_writer = Arc::new(Mutex::new(pair.master.take_writer().expect("pty writer")));
    spawn_dsr_responder(proxy_log.clone(), Arc::clone(&proxy_writer));

    // wait until the proxy actually serves loopback
    let bound = {
        let deadline = Instant::now() + PROXY_READY_TIMEOUT;
        loop {
            if std::net::TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], PROXY_PORT))).is_ok()
            {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    assert!(
        bound,
        "the proxy did not bind {PROXY_ADDR} within {PROXY_READY_TIMEOUT:?}.\n--- proxy console output ---\n{}",
        proxy_log.text()
    );

    proxy_writer
        .lock()
        .expect("pty writer mutex poisoned")
        .write_all(b"/candyriya reload\r\n")
        .expect("writing to the proxy console");
    wait_for(
        RELOAD_TIMEOUT,
        Duration::from_millis(100),
        "/candyriya reload output",
        || {
            proxy_log.contains("config reloaded")
                && proxy_log.contains("plugins reloaded, 0 failed")
        },
    );

    proxy_writer
        .lock()
        .expect("pty writer mutex poisoned")
        .write_all(b"/candyriya end\r\n")
        .expect("writing to the proxy console");
    let deadline = Instant::now() + STOP_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "the proxy did not exit within {STOP_TIMEOUT:?}"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => panic!("polling the proxy exit status: {error}"),
        }
    };
    assert!(
        status.success(),
        "the proxy did not exit cleanly after reload: {status:?}\n--- proxy console output ---\n{}",
        proxy_log.text()
    );
}
