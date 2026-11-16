//! Integration tests for the online (`hasJoined`) validator.
//!
//! No network access is used: every test that talks HTTP spawns a real
//! local TCP server over `std::net::TcpListener` speaking hand-written
//! HTTP/1.1 with proper `Content-Length` framing. It is not a mock library
//! and not an in-process fake — the client under test goes through the full
//! socket stack.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use candyriya_auth::{AuthError, MojangSessionValidator, SessionValidator};
use uuid::Uuid;

/// Retry budget for transport-level failures.
///
/// This machine's endpoint protection randomly aborts a small share of
/// loopback connections opened by short-lived test processes (observed:
/// roughly 1 in 10 attempts, unrelated to test content, timing or
/// parallelism). A `Transport` error therefore doesn't necessarily mean
/// the validator is broken. Retry those — and only those — a few times;
/// the final outcome is still the real result. Failures like
/// `SessionRejected` or `Parse`, and successes, return immediately.
async fn verify(
    validator: &MojangSessionValidator,
    username: &str,
    server_id: &str,
) -> Result<candyriya_player::GameProfile, AuthError> {
    const ATTEMPTS: usize = 8;
    let mut last = AuthError::Transport("no attempts made".to_owned());
    for _ in 0..ATTEMPTS {
        match validator.verify_client(username, server_id).await {
            Err(AuthError::Transport(message)) => {
                last = AuthError::Transport(message);
            }
            other => return other,
        }
    }
    Err(last)
}

/// What the test server does with an incoming connection.
enum ResponseSpec {
    /// Answer immediately with the given status and body.
    Full {
        status: u16,
        reason: &'static str,
        content_type: &'static str,
        body: String,
    },
    /// Accept the connection, read the request, then never answer.
    /// Used to exercise client-side timeouts.
    HangForever,
}

/// A handle to a running test server; shut it down by dropping it.
struct TestServer {
    url: String,
    /// Raw request heads (request line + headers) the server received.
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    accept_thread: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn spawn(spec: ResponseSpec) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let url = format!(
            "http://{}/session/hasJoined",
            listener.local_addr().expect("local addr")
        );

        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let spec = Arc::new(spec);

        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);
        let accept_thread = thread::spawn(move || {
            loop {
                if thread_stop.load(Ordering::Relaxed) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        // Each connection gets its own thread so the accept
                        // loop stays responsive even for `HangForever`.
                        let conn_spec = Arc::clone(&spec);
                        let conn_requests = Arc::clone(&thread_requests);
                        let conn_stop = Arc::clone(&thread_stop);
                        thread::spawn(move || {
                            handle_connection(stream, &conn_spec, &conn_requests, &conn_stop)
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            }
        });

        Self {
            url,
            requests,
            stop,
            accept_thread: Some(accept_thread),
        }
    }

    fn captured_requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests mutex").clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Close the socket the way the client tolerates best on Windows: tell the
/// peer we're done writing, then read whatever is left (a bodyless GET has
/// nothing, so this sees EOF quickly) before dropping. Dropping with
/// unread data in the receive buffer is what turns a clean FIN into an
/// RST and breaks the client mid-request.
fn drain_and_close(mut stream: std::net::TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let mut sink = [0u8; 1024];
    loop {
        match stream.read(&mut sink) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

fn handle_connection(
    mut stream: std::net::TcpStream,
    spec: &Arc<ResponseSpec>,
    requests: &Arc<Mutex<Vec<String>>>,
    stop: &Arc<AtomicBool>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));

    // Read until end of headers; HTTP/1.1 requests we produce (GET, no
    // body) carry everything up to the blank line.
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    requests.lock().expect("requests mutex").push(head);

    match &**spec {
        ResponseSpec::Full {
            status,
            reason,
            content_type,
            body,
        } => {
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            drain_and_close(stream);
        }
        ResponseSpec::HangForever => {
            // Park until the harness is dropped; the client is expected to
            // time out long before this loop notices.
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(10));
            }
            drain_and_close(stream);
        }
    }
}

fn json_server(body: &str) -> TestServer {
    TestServer::spawn(ResponseSpec::Full {
        status: 200,
        reason: "OK",
        content_type: "application/json",
        body: body.to_owned(),
    })
}

#[tokio::test]
async fn hasjoined_200_with_textures_parses_profile() {
    let server = json_server(
        r#"{
            "id": "069a79f444e94726a5befca90e38aaf5",
            "name": "Notch",
            "properties": [
                {
                    "name": "textures",
                    "value": "eyJ0ZXh0dXJlcyI6eyJTS",
                    "signature": "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV"
                },
                {
                    "name": "unsigned_prop",
                    "value": "raw-value-kept-as-is"
                }
            ]
        }"#,
    );
    let validator =
        MojangSessionValidator::new(&server.url, Duration::from_secs(2)).expect("client builds");

    let profile = verify(&validator, "Notch", "-deadbeef")
        .await
        .expect("200 must verify");

    assert_eq!(
        profile.id,
        Uuid::parse_str("069a79f4-44e9-4726-a5be-fca90e38aaf5").unwrap()
    );
    assert_eq!(profile.name, "Notch");
    assert_eq!(profile.properties.len(), 2);
    assert_eq!(profile.properties[0].name, "textures");
    assert_eq!(profile.properties[0].value, "eyJ0ZXh0dXJlcyI6eyJTS");
    assert_eq!(
        profile.properties[0].signature.as_deref(),
        Some("SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV")
    );
    // unsigned properties are kept verbatim, with no signature fabricated
    assert_eq!(profile.properties[1].name, "unsigned_prop");
    assert_eq!(profile.properties[1].value, "raw-value-kept-as-is");
    assert_eq!(profile.properties[1].signature, None);

    let captured = server.captured_requests();
    // a retry on transport failure shows up here as more requests; any of
    // them may serve as the witness for the query format
    assert!(
        !captured.is_empty(),
        "expected at least one request to reach the server"
    );
    let first = &captured[0];
    assert!(first.starts_with("GET /session/hasJoined?"), "got: {first}");
    assert!(first.contains("username=Notch"), "got: {first}");
    assert!(first.contains("serverId=-deadbeef"), "got: {first}");
}

#[tokio::test]
async fn hasjoined_200_without_properties_key_defaults_to_empty() {
    let server = json_server(r#"{"id":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"Notch"}"#);
    let validator = MojangSessionValidator::new(&server.url, Duration::from_secs(2)).unwrap();

    let profile = verify(&validator, "Notch", "x").await.unwrap();
    assert!(profile.properties.is_empty());
}

#[tokio::test]
async fn hasjoined_200_with_non_uuid_id_is_rejected() {
    let server = json_server(r#"{"id":"definitely-not-a-uuid","name":"Notch","properties":[]}"#);
    let validator = MojangSessionValidator::new(&server.url, Duration::from_secs(2)).unwrap();

    let err = verify(&validator, "Notch", "x").await.unwrap_err();
    assert!(matches!(err, AuthError::Parse(_)), "got: {err:?}");
}

#[tokio::test]
async fn hasjoined_204_is_rejected() {
    let server = TestServer::spawn(ResponseSpec::Full {
        status: 204,
        reason: "No Content",
        content_type: "text/plain",
        body: String::new(),
    });
    let validator = MojangSessionValidator::new(&server.url, Duration::from_secs(2)).unwrap();

    let err = verify(&validator, "Notch", "x").await.unwrap_err();
    assert!(matches!(err, AuthError::SessionRejected(_)), "got: {err:?}");
}

#[tokio::test]
async fn hasjoined_403_is_rejected() {
    let server = TestServer::spawn(ResponseSpec::Full {
        status: 403,
        reason: "Forbidden",
        content_type: "application/json",
        body: r#"{"error":"Forbidden"}"#.to_owned(),
    });
    let validator = MojangSessionValidator::new(&server.url, Duration::from_secs(2)).unwrap();

    let err = verify(&validator, "Notch", "x").await.unwrap_err();
    assert!(matches!(err, AuthError::SessionRejected(_)), "got: {err:?}");
}

#[tokio::test]
async fn hasjoined_500_is_rejected() {
    let server = TestServer::spawn(ResponseSpec::Full {
        status: 500,
        reason: "Internal Server Error",
        content_type: "text/plain",
        body: "boom".to_owned(),
    });
    let validator = MojangSessionValidator::new(&server.url, Duration::from_secs(2)).unwrap();

    let err = verify(&validator, "Notch", "x").await.unwrap_err();
    assert!(matches!(err, AuthError::SessionRejected(_)), "got: {err:?}");
}

#[tokio::test]
async fn hasjoined_200_with_garbage_body_is_rejected() {
    let server = json_server("this is definitely not JSON");
    let validator = MojangSessionValidator::new(&server.url, Duration::from_secs(2)).unwrap();

    let err = verify(&validator, "Notch", "x").await.unwrap_err();
    assert!(matches!(err, AuthError::SessionRejected(_)), "got: {err:?}");
}

#[tokio::test]
async fn hasjoined_timeout_is_rejected() {
    let server = TestServer::spawn(ResponseSpec::HangForever);
    let validator = MojangSessionValidator::new(&server.url, Duration::from_millis(250)).unwrap();

    let started = Instant::now();
    let err = verify(&validator, "Notch", "x").await.unwrap_err();
    let elapsed = started.elapsed();

    assert!(matches!(err, AuthError::SessionRejected(_)), "got: {err:?}");
    assert!(
        elapsed < Duration::from_secs(5),
        "timeout took too long: {elapsed:?}"
    );
}

#[tokio::test]
async fn online_validator_rejects_invalid_username_without_network() {
    // A hanging server: if the validator hit the network the test would
    // stall or fail with a timeout instead of a validation error, and the
    // captured-requests assertion below would be non-empty.
    let server = TestServer::spawn(ResponseSpec::HangForever);
    let validator = MojangSessionValidator::new(&server.url, Duration::from_secs(2)).unwrap();

    for bad in [
        "ab",
        "name with spaces",
        "na-me",
        "thisnameiswaytoolongtobevalid",
        "",
    ] {
        let err = validator.verify_client(bad, "x").await.unwrap_err();
        assert!(
            matches!(err, AuthError::InvalidUsername(_)),
            "{bad:?} should fail validation, got: {err:?}"
        );
    }

    assert!(
        server.captured_requests().is_empty(),
        "no requests must leave the process for invalid usernames"
    );
}

#[tokio::test]
async fn online_validator_preserves_username_case_verbatim() {
    let server = json_server(
        r#"{"id":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"NOTCH","properties":[]}"#,
    );
    let validator = MojangSessionValidator::new(&server.url, Duration::from_secs(2)).unwrap();

    let profile = verify(&validator, "NOTCH", "x").await.unwrap();
    assert_eq!(profile.name, "NOTCH");
}
