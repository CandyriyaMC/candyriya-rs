//! Integration tests for the `candyriya` binary.
//!
//! These tests spawn the real binary; no code path is mocked. Nothing here
//! touches the network: every fixture runs with `online-mode = false` and
//! an empty backend list, and the listen fixture binds a random free
//! loopback port found at test time.

use std::io::Read as _;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BINARY: &str = env!("CARGO_BIN_EXE_candyriya");

/// Pick a loopback port that is currently free.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    listener
        .local_addr()
        .expect("read the bound address")
        .port()
}

/// Kill a child process and wait for it, ignoring errors.
fn reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Poll until the proxy answers on `addr`, or give up after `within`.
fn wait_for_connect(addr: std::net::SocketAddr, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(250)) {
            // prove the proxy accepted us by reading nothing and dropping
            let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
            let mut byte = [0u8; 1];
            let _ = stream.read(&mut byte); // times out or EOFs — either is fine
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn write_fixture_config(dir: &Path, port: u16) {
    let config = format!(
        r#"config-version = "0.1"
bind = "127.0.0.1:{port}"
motd = "<aqua>candyriya test"
motd-favicon = "missing-favicon.png"
online-mode = false
player-info-forwarding-mode = "modern"

[servers]
try = []

[advanced]
max-connections = 16
handshake-timeout = 5000

[metrics]
prometheus = false
"#
    );
    std::fs::write(dir.join("candyriya.toml"), config).expect("write fixture config");
}

#[test]
fn version_prints_version_and_banner() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = Command::new(BINARY)
        .arg("--version")
        .current_dir(dir.path())
        .output()
        .expect("run --version");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("version output is utf-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "version output should be exactly two lines: {stdout:?}"
    );

    let first = lines[0];
    assert!(
        first.starts_with("candyriya "),
        "unexpected version line: {first:?}"
    );
    let rest = first
        .strip_prefix("candyriya ")
        .and_then(|rest| rest.strip_suffix(')'))
        .expect("version line ends with the build marker");
    let (version, build) = rest
        .rsplit_once(" (build ")
        .expect("version line carries the build marker");
    assert!(!version.is_empty(), "version is empty in {first:?}");
    assert!(
        build.chars().all(|c| c.is_ascii_alphanumeric()) && (1..=64).contains(&build.len()),
        "unexpected build marker {build:?} in {first:?}"
    );

    let banner = lines[1];
    assert!(!banner.is_empty(), "banner line is empty");
    assert!(
        banner.contains("Candyriya"),
        "unexpected banner: {banner:?}"
    );
}

#[test]
fn help_exits_successfully() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = Command::new(BINARY)
        .arg("--help")
        .current_dir(dir.path())
        .output()
        .expect("run --help");
    assert!(
        output.status.success(),
        "help must exit with 0: {:?}",
        output.status
    );
    let stdout = String::from_utf8(output.stdout).expect("help output is utf-8");
    assert!(
        stdout.contains("Usage"),
        "help output lacks usage: {stdout:?}"
    );
    assert!(
        stdout.contains("--config"),
        "help output lacks --config: {stdout:?}"
    );
}

#[test]
fn missing_explicit_config_is_a_clean_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist.toml");
    let output = Command::new(BINARY)
        .arg("--config")
        .arg(&missing)
        // telemetry initializes before the config error is reported; keep
        // the daily log out of the source tree
        .current_dir(dir.path())
        .output()
        .expect("run with a missing --config path");
    assert!(
        !output.status.success(),
        "a missing --config must fail with a non-zero exit code"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("configuration file"),
        "expected a useful diagnostic, got: {combined}"
    );
    assert!(
        combined.contains("does-not-exist.toml"),
        "expected the diagnostic to name the path, got: {combined}"
    );
    assert!(
        !combined.contains("panicked"),
        "a missing config must not panic: {combined}"
    );
}

#[test]
fn missing_default_config_is_created_from_the_template() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut child = Command::new(BINARY)
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the proxy without a config file");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut created = false;
    while Instant::now() < deadline {
        if dir.path().join("candyriya.toml").is_file() {
            created = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    reap(&mut child);

    assert!(
        created,
        "a missing default ./candyriya.toml should be created from the embedded template"
    );
    let content = std::fs::read_to_string(dir.path().join("candyriya.toml"))
        .expect("read the generated default config");
    assert!(
        content.contains("config-version"),
        "template looks wrong: {content:?}"
    );
}

#[test]
fn proxy_binds_and_accepts_a_connection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let port = free_port();
    write_fixture_config(dir.path(), port);

    let mut child = Command::new(BINARY)
        .arg("--config")
        .arg(dir.path().join("candyriya.toml"))
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the proxy");

    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().expect("fixture addr");
    let connected = wait_for_connect(addr, Duration::from_secs(15));

    // the daily log file must exist with the configured directory layout
    let log_exists = dir.path().join("logs").is_dir();

    reap(&mut child);

    assert!(connected, "the proxy did not accept connections on {addr}");
    assert!(
        log_exists,
        "expected ./logs to be created for the rotating log"
    );
}

#[test]
fn version_flag_short_circuits_before_config_handling() {
    // --version must not require a config file to exist or be valid
    let dir = tempfile::tempdir().expect("tempdir");
    let output = Command::new(BINARY)
        .arg("--version")
        .current_dir(dir.path())
        .output()
        .expect("run --version from a bare directory");
    assert!(output.status.success());
}
