//! Wake-the-stack smoke test: a real Paper 1.21.1 backend, the real
//! `candyriya` binary, and a real client on the wire.
//!
//! Unlike the loopback integration suite, this exercises everything the
//! fixture cannot reproduce faithfully: a real JVM backend with world
//! loading and Paper's own protocol handlers, modern forwarding against
//! Paper's HMAC check (a mismatch shows up as a kick, nothing else), zlib
//! framing handed to the client via Set Compression, the proxy's console
//! REPL (only spawned on a TTY, so driven through a pseudo-terminal here),
//! and both processes stopping on their own terms.
//!
//! The test is **off by default**: it downloads a ~49 MB Paper jar on first
//! run (cached in `target/smoke/` afterwards) and needs a Java 21+ runtime.
//! It is gated two ways — `#[ignore]` without `--features smoke`, and the
//! `CANDYRIYA_SMOKE=1` environment variable at run time — so neither CI nor a
//! casual `cargo test --workspace --all-features` touches the network or a
//! JVM. See `README.md` in this crate for the full recipe.
//!
//! Secrets: a fresh forwarding secret is generated on every run and written
//! into the (untracked, throwaway) run directories under `target/smoke/`.
//! No secret in this repository is ever used, and none of the values this
//! test produces are committed. The committed `paper-server/` tree carries a
//! `SMOKE_SECRET_PLACEHOLDER` marker that is substituted here at launch.
//!
//! Known deviations from the ideal (documented in the crate README):
//!
//! * The proxy runs with `online-mode = false`. Online mode verifies the
//!   client against Mojang's session server (`hasJoined`), which cannot
//!   succeed without a real Microsoft account — the harness client would be
//!   rejected before ever reaching the backend. Offline mode is the only
//!   configuration under which "the echoed UUID equals the vanilla
//!   offline-mode derivation" is meaningful, and it keeps the modern
//!   forwarding path (the risky part) fully exercised end to end.
//! * The ping is answered by the proxy from its own configuration; this
//!   proxy never forwards status requests to a backend, so "backend ping"
//!   cannot be asserted here. The assertion covers the proxy's status path,
//!   the configured MOTD and the max-players in `candyriya.toml`, and the
//!   protocol echo — everything a client-visible ping can observe.
//! * "SIGTERM" is not a thing on Windows, and on Unix the proxy only breaks
//!   out of its main loop on Ctrl-C (SIGINT). Both processes are therefore
//!   asked to stop the way they actually support — the proxy gets
//!   `/candyriya end` through its console, Paper gets `stop` on its stdin —
//!   and the test asserts they exit by themselves with code 0.

#[cfg_attr(not(feature = "smoke"), ignore)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paper_backend_smoke() {
    if std::env::var_os("CANDYRIYA_SMOKE").is_none() {
        eprintln!(
            "skipping the Paper smoke test: set CANDYRIYA_SMOKE=1 to run it \
             (see crates/it/candyriya-it/README.md)"
        );
        return;
    }
    run_smoke().await;
}

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use candyriya_it::{Intent, TestClient};
use candyriya_player::offline_uuid;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use sha2::{Digest, Sha256};

/// The exact object the smoke runs against. The URL pins the SHA-256 of the
/// jar as well (fill-data's object key *is* the checksum); the downloaded
/// file is verified against it byte for byte and any mismatch is a hard
/// failure, not a warning.
const PAPER_URL: &str = "https://fill-data.papermc.io/v1/objects/39bd8c00b9e18de91dcabd3cc3dcfa5328685a53b7187a2f63280c22e2d287b9/paper-1.21.1-133.jar";
const PAPER_SHA256: &str = "39bd8c00b9e18de91dcabd3cc3dcfa5328685a53b7187a2f63280c22e2d287b9";
const PAPER_JAR_NAME: &str = "paper-1.21.1-133.jar";
/// Committed placeholder in `paper-server/config/paper-global.yml`; replaced
/// with the per-run secret before launch.
const SECRET_PLACEHOLDER: &str = "SMOKE_SECRET_PLACEHOLDER";

const PAPER_ADDR: &str = "127.0.0.1:25565";
const PROXY_ADDR: &str = "127.0.0.1:25569";
const PROXY_PORT: u16 = 25569;
const PAPER_READY_TIMEOUT: Duration = Duration::from_secs(150);
const PROXY_READY_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const PAPER_STOP_TIMEOUT: Duration = Duration::from_secs(90);

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

fn smoke_dir() -> PathBuf {
    workspace_root().join("target").join("smoke")
}

fn fresh_dir(path: &Path) -> PathBuf {
    if path.exists() {
        std::fs::remove_dir_all(path).expect("clearing the previous smoke run");
    }
    std::fs::create_dir_all(path).expect("creating the smoke run directory");
    path.to_path_buf()
}

fn sha256_of_file(path: &Path) -> String {
    let bytes =
        std::fs::read(path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
    format!("{:x}", Sha256::digest(&bytes))
}

/// Locate the `candyriya` binary: `$CANDYRIYA_SMOKE_BIN` when set, otherwise
/// `target/debug/candyriya[.exe]` and `target/release/candyriya[.exe]`. The
/// smoke deliberately does not build the workspace itself, but it refuses to
/// guess silently.
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

/// Ensure the Paper jar is present and intact; download it when missing.
/// Any checksum mismatch is fatal — silently proceeding would verify the
/// proxy against something other than the pinned, known-good artifact.
async fn ensure_paper_jar() -> PathBuf {
    let jar = smoke_dir().join(PAPER_JAR_NAME);
    if !jar.exists() {
        eprintln!(
            "smoke: downloading paper-1.21.1-133 (~49 MB) to {}",
            jar.display()
        );
        let partial = jar.with_extension("jar.partial");
        let response = reqwest::get(PAPER_URL)
            .await
            .unwrap_or_else(|error| panic!("downloading {PAPER_URL}: {error}"));
        let status = response.status();
        assert!(status.is_success(), "paper jar download returned {status}");
        let bytes = response
            .bytes()
            .await
            .unwrap_or_else(|error| panic!("reading the jar response body: {error}"));
        std::fs::create_dir_all(smoke_dir()).expect("creating target/smoke");
        std::fs::write(&partial, &bytes).expect("writing the jar to a temporary file");
        std::fs::rename(&partial, &jar).expect("moving the jar into place");
    } else {
        eprintln!("smoke: reusing cached jar {}", jar.display());
    }

    let hash = sha256_of_file(&jar);
    assert_eq!(
        hash,
        PAPER_SHA256,
        "paper jar checksum mismatch at {} — delete the file and re-run to re-download",
        jar.display()
    );
    jar
}

/// A fresh 32-character printable secret. Both sides (the proxy's
/// `forwarding.secret` and Paper's `proxies.velocity.secret`) use the
/// UTF-8 bytes of this string as the HMAC key, so the same string is what
/// both files carry.
fn fresh_forwarding_secret() -> String {
    const ALPHABET: &[u8; 62] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    uuid::Uuid::new_v4()
        .into_bytes()
        .into_iter()
        .chain(uuid::Uuid::new_v4().into_bytes())
        .take(32)
        .map(|byte| ALPHABET[(byte % 62) as usize] as char)
        .collect()
}

/// Copy the committed `paper-server/` template into `target_dir`, swapping
/// the secret placeholder in `config/paper-global.yml` for `secret`.
fn materialize_backend(target_dir: &Path, secret: &str) {
    let template = workspace_root().join("paper-server");
    for entry in ["eula.txt", "server.properties", "spigot.yml"] {
        std::fs::copy(template.join(entry), target_dir.join(entry))
            .unwrap_or_else(|error| panic!("copying {entry}: {error}"));
    }
    let global = std::fs::read_to_string(template.join("config").join("paper-global.yml"))
        .expect("reading the paper-global.yml template");
    assert!(
        global.contains(SECRET_PLACEHOLDER),
        "the paper-global.yml template no longer carries the {SECRET_PLACEHOLDER} placeholder"
    );
    let global = global.replace(SECRET_PLACEHOLDER, secret);
    let config_dir = target_dir.join("config");
    std::fs::create_dir_all(&config_dir).expect("creating the backend config directory");
    std::fs::write(config_dir.join("paper-global.yml"), global)
        .expect("writing the backend paper-global.yml");
}

fn proxy_config(secret_file_name: &str) -> String {
    format!(
        r#"# Generated by the smoke test on every run; never committed.
config-version = "0.1"
bind = "{PROXY_ADDR}"
motd = "<green>smoke"
show-max-players = 16
online-mode = false
player-info-forwarding-mode = "modern"
forwarding-secret-file = "{secret_file_name}"

[protocol]
# the smoke must not depend on GitHub being reachable
auto-update = false

[servers]
try = ["local"]
local = "{PAPER_ADDR}"

[forced-hosts]
"play.example.com" = ["local"]

[advanced]
"#
    )
}

/// Everything a child process printed, shared with the reader thread.
#[derive(Clone, Default)]
struct Log {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Log {
    fn spawn_reader<R: Read + Send + 'static>(reader: R) -> Self {
        let log = Self::default();
        let shared = Arc::clone(&log.bytes);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => match shared.lock() {
                        Ok(mut bytes) => bytes.extend_from_slice(&buf[..n]),
                        // a poisoned log means the test panicked already;
                        // dropping the reader thread's data on the floor is fine
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

    fn tail(&self, lines: usize) -> String {
        self.text()
            .lines()
            .rev()
            .take(lines)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
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

/// Kill-on-drop backstop for the backend process; the happy path reaps it
/// explicitly (after which `kill` is a no-op on an already-exited child).
struct PaperProcess(std::process::Child);

impl Drop for PaperProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct PtyProcess {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    log: Log,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

/// A real terminal answers a `ESC[6n` (device status report — "what is your
/// cursor position?") with `ESC[{row};{col}R`. Windows pseudoconsoles forward
/// the query from console API calls (e.g. `GetConsoleScreenBufferInfo`,
/// which console-aware ANSI writers issue at construction) and the call
/// blocks until the answer arrives — so a pty that never answers leaves the
/// child stuck before its first log line. This watcher plays the terminal:
/// every `ESC[6n` it sees in the child's output gets one CPR back, however
/// many times the child asks.
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
                        // one CPR per DSR; row/col values are uninterpreted
                        let _ = writer.write_all(b"\x1b[1;1R");
                    }
                }
                answered += pending;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        let _ = self.child.try_wait();
        // no-op when the child already exited; SIGKILL otherwise
        let _ = self.child.kill();
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

async fn run_smoke() {
    let jar = ensure_paper_jar().await;
    let work = smoke_dir();
    let backend_dir = fresh_dir(&work.join("run-backend"));
    let proxy_dir = fresh_dir(&work.join("run-proxy"));

    let secret = fresh_forwarding_secret();
    materialize_backend(&backend_dir, &secret);

    // the proxy side: candyriya.toml and a base64-encoded copy of the same
    // secret bytes (the proxy decodes this file; Paper was configured with
    // the plain string, so the decoded HMAC keys agree)
    {
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD as BASE64;
        let encoded = BASE64.encode(secret.as_bytes());
        std::fs::write(proxy_dir.join("forwarding.secret"), format!("{encoded}\n"))
            .expect("writing the proxy forwarding.secret");
        std::fs::write(
            proxy_dir.join("candyriya.toml"),
            proxy_config("forwarding.secret"),
        )
        .expect("writing the proxy candyriya.toml");
    }

    // ---- launch the backend
    eprintln!(
        "smoke: launching Paper (java -Xmx1G -jar {} nogui)",
        jar.display()
    );
    let paper = Command::new("java")
        .arg("-Xmx1G")
        .arg("-jar")
        .arg(&jar)
        .arg("nogui")
        .current_dir(&backend_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to launch java — is a JRE on PATH? {error}"));
    let mut paper = PaperProcess(paper);
    let paper_stdout = Log::spawn_reader(paper.0.stdout.take().expect("piped paper stdout"));
    let paper_stderr = Log::spawn_reader(paper.0.stderr.take().expect("piped paper stderr"));
    let mut paper_stdin = paper.0.stdin.take().expect("piped paper stdin");

    let started = Instant::now();
    wait_for(
        PAPER_READY_TIMEOUT,
        Duration::from_millis(250),
        "Paper to become ready",
        || {
            // cheap liveness check first: if the JVM died during boot, fail now
            // instead of polling a dead log for two minutes
            if let Ok(Some(status)) = paper.0.try_wait() {
                panic!(
                    "Paper exited early with {status}.\n--- stdout tail ---\n{}\n--- stderr tail ---\n{}",
                    paper_stdout.tail(60),
                    paper_stderr.tail(60)
                );
            }
            paper_stdout.contains("Done (")
        },
    );
    eprintln!("smoke: paper ready in {:.1?}", started.elapsed());

    // ---- launch the proxy on a pseudo-terminal; the interactive console is
    // only spawned when stdin is a TTY
    eprintln!("smoke: launching candyriya (cwd {})", proxy_dir.display());
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
    command.arg(proxy_dir.join("candyriya.toml"));
    command.cwd(&proxy_dir);
    let child = pair
        .slave
        .spawn_command(command)
        .expect("spawning candyriya on the pty");
    drop(pair.slave);
    let proxy_log = Log::spawn_reader(pair.master.try_clone_reader().expect("pty reader"));
    let proxy_writer = Arc::new(Mutex::new(pair.master.take_writer().expect("pty writer")));
    spawn_dsr_responder(proxy_log.clone(), Arc::clone(&proxy_writer));
    let mut proxy = PtyProcess {
        child,
        writer: proxy_writer,
        log: proxy_log,
        _master: pair.master,
    };

    let started = Instant::now();
    let proxy_bound = {
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
        proxy_bound,
        "the proxy did not bind {PROXY_ADDR} within {PROXY_READY_TIMEOUT:?}.\n--- proxy console output ---\n{}",
        proxy.log.tail(80)
    );
    eprintln!("smoke: proxy listening after {:.1?}", started.elapsed());

    // ---- the wire: handshake, login, and the forwarding round-trip
    let proxy_addr: SocketAddr = PROXY_ADDR.parse().expect("proxy address");
    let mut client = TestClient::connect(proxy_addr)
        .await
        .expect("connecting to the proxy");
    client
        .send_handshake(767, "127.0.0.1", PROXY_PORT, Intent::Login)
        .await
        .expect("handshake");
    client.login_start(767, "Steve").await.expect("login start");

    let success = tokio::time::timeout(LOGIN_TIMEOUT, client.expect_login_success(767))
        .await
        .expect("timed out waiting for login success")
        .expect("login success");
    eprintln!(
        "smoke: login success for {} (uuid {})",
        success.name(),
        success.uuid_string()
    );

    assert_eq!(success.name(), "Steve");
    let vanilla = offline_uuid("Steve");
    let (msb, lsb) = vanilla.as_u64_pair();
    assert_eq!(
        success.uuid_parts(),
        (msb as i64, lsb as i64),
        "the forwarded UUID must equal the vanilla offline-mode derivation of \"Steve\""
    );
    assert_eq!(
        client.compression_threshold(),
        Some(256),
        "the link must have negotiated Paper's default compression threshold"
    );

    // ---- a second, independent connection: the status exchange through the
    // proxy (the proxy answers pings itself; no backend is involved)
    let mut ping_client = TestClient::connect(proxy_addr)
        .await
        .expect("connecting for the status ping");
    ping_client
        .send_handshake(767, "play.example.com", PROXY_PORT, Intent::Status)
        .await
        .expect("status handshake");
    let raw = ping_client
        .status_request(767)
        .await
        .expect("status response");
    let status: serde_json::Value = serde_json::from_str(&raw).expect("status response is JSON");
    assert_eq!(
        status["players"]["max"], 16,
        "the configured show-max-players must surface in the ping"
    );
    assert_eq!(
        status["version"]["protocol"], 767,
        "the ping must echo the client protocol"
    );
    assert!(
        raw.contains("smoke"),
        "the configured <green>smoke MOTD must surface in the ping; got {raw}"
    );
    let pong = ping_client
        .status_ping(767, 0x0122_3344_5566_7788)
        .await
        .expect("status pong");
    assert_eq!(
        pong, 0x0122_3344_5566_7788,
        "the ping payload must be echoed verbatim"
    );
    drop(ping_client);

    // ---- the backend log must show the proxy-mediated login. Paper does
    // not spawn the player entity — the harness client does not answer the
    // post-1.20.2 configuration handshake, so "logged in with entity id"
    // never happens — but it does print the forwarded profile, and it only
    // prints it after the modern-forwarding HMAC verified against the shared
    // secret. That line is the strongest available marker of a proxied join.
    let forwarded = success.uuid_string();
    wait_for(
        Duration::from_secs(10),
        Duration::from_millis(100),
        "the backend log to record the forwarded profile",
        || paper_stdout.contains(&format!("UUID of player Steve is {forwarded}")),
    );
    eprintln!("smoke: backend log recorded the forwarded profile ({forwarded})");

    // ---- the proxy's console must answer /candyriya info over the pty
    proxy
        .writer
        .lock()
        .expect("pty writer mutex poisoned")
        .write_all(b"/candyriya info\r\n")
        .expect("writing to the proxy console");
    wait_for(
        Duration::from_secs(15),
        Duration::from_millis(100),
        "/candyriya info output",
        || proxy.log.contains("Candyriya") && proxy.log.contains("Uptime:"),
    );
    eprintln!("smoke: /candyriya info answered over the console");

    // ---- shut both down the way they support, and require clean exits
    proxy
        .writer
        .lock()
        .expect("pty writer mutex poisoned")
        .write_all(b"/candyriya end\r\n")
        .expect("asking the proxy to stop");
    let deadline = Instant::now() + STOP_TIMEOUT;
    let proxy_status = loop {
        match proxy.child.try_wait() {
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
        proxy_status.success(),
        "the proxy did not exit cleanly: {proxy_status:?}"
    );
    eprintln!("smoke: proxy exited cleanly");

    paper_stdin
        .write_all(b"stop\n")
        .expect("writing stop to paper's console");
    drop(paper_stdin);
    let deadline = Instant::now() + PAPER_STOP_TIMEOUT;
    let paper_status = loop {
        match paper.0.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "Paper did not exit within {PAPER_STOP_TIMEOUT:?}"
                );
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(error) => panic!("polling the backend exit status: {error}"),
        }
    };
    assert!(
        paper_status.success(),
        "Paper did not exit cleanly: {paper_status:?}"
    );
    eprintln!("smoke: paper exited cleanly");

    eprintln!("smoke: ALL CHECKS PASSED (proxy at {PROXY_ADDR}, backend at {PAPER_ADDR})");
}
